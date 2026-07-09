//! Главный обработчик запроса MCP `tools/call`.
//!
//! Сюжет одного вызова:
//! 1. Решить, кэшировать ли вообще (политика → cacheable / TTL).
//! 2. Если нет — форвард напрямую, не сохраняем.
//! 3. Иначе — стабильный ключ → cache.get(). Если жив — return (cache hit).
//! 4. Иначе — single-flight: один поток идёт в бэкенд, остальные ждут.
//! 5. Записываем ответ в кэш с TTL, возвращаем.
//!
//! Этот модуль НЕ знает про HTTP/axum/rmcp — он работает на уровне «дай мне
//! ответ на (tool, args)». Конкретный транспорт (HTTP MCP или CLI) подключают
//! бинарники.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tracing::{debug, warn};

use crate::cache::Cache;
use crate::dirty::DirtySet;
use crate::freeze::FreezeController;
use crate::metrics::Metrics;
use crate::policy::Policy;
use crate::singleflight::SingleFlight;

/// Стандартизированный ответ кэш-прокси: либо текст JSON-ответа MCP-tool'а,
/// либо ошибка пути (бэкенд недоступен / сериализация и т.п.).
pub type ProxyResult = Result<Arc<String>, ProxyError>;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("ошибка обращения к бэкенду: {0}")]
    Backend(String),
    #[error("внутренняя ошибка прокси: {0}")]
    Internal(String),
    /// Прокси заморожен (block-режим): обновление конфигурации в процессе.
    /// Клиент должен повторить через `retry_after_seconds` секунд.
    /// Старые кэшированные ответы НЕ отдаются — намеренно, чтобы избежать
    /// рассинхрона между новой структурой БД и старым индексом кода.
    #[error("прокси заморожен (scope='{scope}', retry after {retry_after_seconds}s)")]
    Frozen {
        scope: String,
        retry_after_seconds: u64,
    },
}

/// Абстракция: «как сходить в бэкенд за ответом на (tool, args)». Параметризуем
/// чтобы тесты могли подсунуть фейк, а реальный бинарник — `reqwest`-клиент к
/// HTTP MCP-эндпоинту.
#[async_trait::async_trait]
pub trait BackendCaller: Send + Sync + 'static {
    async fn call(&self, tool: &str, args: &Value) -> Result<String, String>;
}

/// Параметры write-triggered ленивой ревалидации (#1471).
#[derive(Debug, Clone)]
pub struct RevalConfig {
    /// Учитывать ли dirty-флаги и сверять mtime перед запоминанием ответа.
    /// `false` — старое поведение (только TTL + `POST /invalidate`).
    pub enabled: bool,
    /// Бюджет ожидания догона индекса для грязного чтения (strong-режим).
    /// `0` → eventual: один форвард без ретраев.
    pub max_wait: Duration,
    /// Пауза между ретраями форварда в strong-режиме.
    pub retry_interval: Duration,
}

impl Default for RevalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_wait: Duration::from_millis(2000),
            retry_interval: Duration::from_millis(150),
        }
    }
}

/// Кэш-прокси: связывает кэш, политику, single-flight и backend-caller.
///
/// `scope_args` — имена полей в args, по которым строится scope-prefix кэш-ключа
/// и проверяется заморозка. Проверяются по порядку, берётся первое найденное
/// строковое поле. Пустой список — scope всегда пустой (`""`); инвалидация по
/// области такие записи не задевает (но `all:true` снесёт).
pub struct CacheProxy<B: BackendCaller> {
    pub server_alias: String,
    pub scope_args: Vec<String>,
    pub policy: Arc<arc_swap::ArcSwap<Policy>>,
    pub cache: Arc<Cache>,
    pub singleflight: Arc<SingleFlight<String>>,
    pub metrics: Arc<Metrics>,
    pub backend: Arc<B>,
    pub freeze: FreezeController,
    /// Множество грязных путей для ленивой ревалидации (#1471). Наполняется
    /// `POST /mark-dirty`; используется в `handle` для решения «отдать из кэша
    /// или форварднуть и сверить mtime».
    pub dirty: Arc<DirtySet>,
    /// Параметры ленивой ревалидации.
    pub reval: RevalConfig,
}

impl<B: BackendCaller> CacheProxy<B> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server_alias: impl Into<String>,
        scope_args: Vec<String>,
        policy: Arc<arc_swap::ArcSwap<Policy>>,
        cache: Arc<Cache>,
        singleflight: Arc<SingleFlight<String>>,
        metrics: Arc<Metrics>,
        backend: Arc<B>,
        freeze: FreezeController,
        dirty: Arc<DirtySet>,
        reval: RevalConfig,
    ) -> Self {
        Self {
            server_alias: server_alias.into(),
            scope_args,
            policy,
            cache,
            singleflight,
            metrics,
            backend,
            freeze,
            dirty,
            reval,
        }
    }

    /// Извлечь scope из args: перебираем `scope_args` по порядку и берём первое
    /// поле, которое есть в args и является строкой. Ничего не нашли (или список
    /// пуст) — область пустая.
    fn extract_scope(&self, args: &Value) -> String {
        for name in &self.scope_args {
            if let Some(s) = args.get(name).and_then(|v| v.as_str()) {
                return s.to_string();
            }
        }
        String::new()
    }

    /// Точка входа: вернуть ответ на (tool, args) — из кэша или из бэкенда.
    /// Параметр `bypass` — клиент явно попросил миновать кэш.
    pub async fn handle(&self, tool: &str, args: &Value, bypass: bool) -> ProxyResult {
        let scope = self.extract_scope(args);

        // Заморозка проверяется ДО политики/кэша/bypass'а — block-режим
        // должен срабатывать на любые `tools/call`, чтобы клиент не получил
        // ни старого ответа из кэша, ни свежего «но устаревшего» от backend'а.
        if let Some(remaining) = self.freeze.is_frozen(&scope) {
            return Err(ProxyError::Frozen {
                scope,
                retry_after_seconds: remaining.as_secs().max(1),
            });
        }

        let policy = self.policy.load();

        // Scope помечен `cacheable = false` в `[scopes.<alias>]` — форвардим
        // напрямую, ответ не сохраняем. Целевой use case — federated репо при
        // групповой работе: запросы идут direct через federation forward без
        // риска stale reads.
        if !scope.is_empty() && !policy.is_scope_cacheable(&scope) {
            self.metrics.record_bypass();
            debug!(tool = %tool, scope = %scope, "scope cacheable=false → bypass");
            return self.forward_no_cache(tool, args).await;
        }

        let ttl = policy.ttl_for(tool);

        // Tool помечен non-cacheable → форвард без сохранения.
        if ttl.is_none() {
            self.metrics.record_bypass();
            return self.forward_no_cache(tool, args).await;
        }

        // Клиент явно сказал «без кэша» → форвард, не пишем.
        if bypass {
            self.metrics.record_bypass();
            return self.forward_no_cache(tool, args).await;
        }

        let key = Cache::key_for(&self.server_alias, &scope, tool, args);
        let ttl = Duration::from_secs(ttl.unwrap_or(0).max(1));

        // Чистый HIT: запись жива И ни один её зависимый файл не «грязный» в этом
        // scope. При выключенной ленивой ревалидации dirty всегда пуст → обычный
        // hit без накладных расходов (any_dirty имеет быстрый путь на пустом set).
        if let Some((payload, deps)) = self.cache.get_with_deps(&key) {
            if !self.reval.enabled || !self.dirty.any_dirty(&scope, &deps) {
                self.metrics.record_hit();
                debug!(tool = %tool, "cache hit");
                return Ok(payload);
            }
            debug!(tool = %tool, scope = %scope, "dirty hit → ревалидация");
        }

        // MISS либо dirty-HIT → идём в бэкенд.
        self.metrics.record_miss();

        // Ленивая ревалидация выключена → старое поведение: один форвард + insert.
        if !self.reval.enabled {
            return self.forward_and_cache(&key, tool, args, ttl).await;
        }

        // Strong с ограниченным бюджетом: форвардим и сверяем mtime; кэшируем
        // ТОЛЬКО когда индекс догнал диск (`index_mtime >= observed`). По
        // исчерпании бюджета — мягкий фолбэк: отдать ответ без запоминания, dirty
        // оставить (снимется на следующем чтении). `max_wait=0` → eventual.
        let deadline = Instant::now() + self.reval.max_wait;
        loop {
            let payload = self.forward_once(&key, tool, args).await?;
            let deps = extract_dependent_files(&payload);
            let mtimes = extract_file_mtimes(&payload);
            // _meta использован для deps/mtimes — снимаем перед кэшем/отдачей.
            let clean = Arc::new(strip_meta(&payload));

            // Среди зависимых файлов смотрим только грязные в этом scope: догнал
            // ли их индекс. Файл без mtime в ответе (нет в file_mtimes) считаем
            // «не догнал» — консервативно, не кэшируем.
            let mut all_caught = true;
            let mut caught: Vec<(String, i64)> = Vec::new();
            for f in &deps {
                let Some(observed) = self.dirty.observed(&scope, f) else {
                    continue;
                };
                match mtimes.get(f) {
                    Some(idx) if *idx >= observed => caught.push((f.clone(), *idx)),
                    _ => all_caught = false,
                }
            }

            if all_caught {
                if deps.is_empty() {
                    self.cache.insert(key.clone(), clean.clone(), ttl);
                } else {
                    self.cache
                        .insert_with_deps(key.clone(), clean.clone(), ttl, deps);
                }
                for (f, idx) in caught {
                    self.dirty.clear_if_caught_up(&scope, &f, idx);
                }
                self.metrics.update_cache_size(self.cache.len());
                return Ok(clean);
            }

            if Instant::now() >= deadline {
                debug!(
                    tool = %tool, scope = %scope,
                    "ревалидация: индекс не догнал в бюджет → отдаю без кэша"
                );
                return Ok(clean);
            }
            tokio::time::sleep(self.reval.retry_interval).await;
        }
    }

    /// Один форвард на бэкенд через single-flight (без записи в кэш). Возвращает
    /// `Arc<String>` payload или ошибку бэкенда.
    async fn forward_once(
        &self,
        key: &str,
        tool: &str,
        args: &Value,
    ) -> Result<Arc<String>, ProxyError> {
        let work = {
            let backend = self.backend.clone();
            let metrics = self.metrics.clone();
            let tool_owned = tool.to_string();
            let args_owned = args.clone();
            move || async move {
                let started = Instant::now();
                let payload = backend.call(&tool_owned, &args_owned).await?;
                metrics.observe_backend_latency_micros(started.elapsed().as_micros() as u64);
                Ok::<String, String>(payload)
            }
        };
        match self.singleflight.do_or_join(key.to_string(), work).await {
            Ok(shared) => Ok(shared),
            Err(err) => {
                self.metrics.record_backend_error();
                warn!(tool = %tool, error = %err, "backend call failed");
                Err(ProxyError::Backend(err))
            }
        }
    }

    /// Форвард + безусловная запись в кэш (с reverse_index по dependent_files).
    /// Путь при выключенной ленивой ревалидации — поведение до #1471.
    async fn forward_and_cache(
        &self,
        key: &str,
        tool: &str,
        args: &Value,
        ttl: Duration,
    ) -> ProxyResult {
        let payload = self.forward_once(key, tool, args).await?;
        let deps = extract_dependent_files(&payload);
        // _meta уже использован для deps — снимаем его перед кэшированием и
        // отдачей клиенту (служебный канал serve↔cache-ci, модели не нужен).
        let clean = Arc::new(strip_meta(&payload));
        if deps.is_empty() {
            self.cache.insert(key.to_string(), clean.clone(), ttl);
        } else {
            self.cache
                .insert_with_deps(key.to_string(), clean.clone(), ttl, deps);
        }
        self.metrics.update_cache_size(self.cache.len());
        Ok(clean)
    }

    async fn forward_no_cache(&self, tool: &str, args: &Value) -> ProxyResult {
        let started = Instant::now();
        match self.backend.call(tool, args).await {
            Ok(payload) => {
                self.metrics
                    .observe_backend_latency_micros(started.elapsed().as_micros() as u64);
                Ok(Arc::new(strip_meta(&payload)))
            }
            Err(err) => {
                self.metrics.record_backend_error();
                Err(ProxyError::Backend(err))
            }
        }
    }
}

/// Снять служебное поле `_meta` из payload перед отдачей клиенту. `_meta`
/// (dependent_files / file_mtimes) — служебный канал serve↔cache-ci: deps идут
/// в reverse_index, mtimes — во write-triggered ревалидацию (#1471). Клиенту
/// (модели) это поле не нужно и только раздувает контекст (на list_files путь
/// каждого файла дублировался ×3). deps/mtimes к моменту вызова уже извлечены.
/// Зеркало [`extract_dependent_files`]: MCP CallToolResult (`content[*].text`
/// со вложенным JSON) или top-level. При любой неожиданности payload не меняется.
fn strip_meta(payload: &str) -> String {
    let mut v: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return payload.to_string(),
    };
    let mut changed = false;
    let is_mcp = v.get("content").map(|c| c.is_array()).unwrap_or(false);
    if is_mcp {
        // MCP CallToolResult: наш _meta лежит во вложенном JSON content[*].text.
        // Top-level `_meta` (поле протокола rmcp) не трогаем.
        if let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) {
            for item in content.iter_mut() {
                let text = match item.get("text").and_then(|t| t.as_str()) {
                    Some(t) => t.to_string(),
                    None => continue,
                };
                let mut inner: Value = match serde_json::from_str(&text) {
                    Ok(iv) => iv,
                    Err(_) => continue,
                };
                let removed = inner
                    .as_object_mut()
                    .map(|o| o.remove("_meta").is_some())
                    .unwrap_or(false);
                if removed {
                    if let Ok(reser) = serde_json::to_string(&inner) {
                        item["text"] = Value::String(reser);
                        changed = true;
                    }
                }
            }
        }
    } else if let Some(obj) = v.as_object_mut() {
        // Top-level форма (non-MCP бэкенд): `_meta` рядом с `result`.
        if obj.remove("_meta").is_some() {
            changed = true;
        }
    }
    // structuredContent (rmcp CallToolResult, structured output): extension-tools
    // serve отдают `{_meta, result}` ещё и здесь, дублируя content[*].text.
    // Без этой ветки `_meta` доезжал до клиента через structuredContent
    // (WS-3, найдено на живом ut-test 2026-06-09: get_object_structure).
    if let Some(sc) = v
        .get_mut("structuredContent")
        .and_then(|s| s.as_object_mut())
    {
        if sc.remove("_meta").is_some() {
            changed = true;
        }
    }
    if changed {
        serde_json::to_string(&v).unwrap_or_else(|_| payload.to_string())
    } else {
        payload.to_string()
    }
}

/// Извлечь список файлов из `_meta.dependent_files` JSON-ответа бэкенда.
/// Поле опционально — бэкенды без поддержки event-based invalidation его не
/// шлют. Невалидный JSON или отсутствие `_meta`/`dependent_files` → пустой
/// вектор (entry кешируется как обычно, чистка идёт только по TTL).
///
/// Cache-ci проксирует MCP `tools/call`, поэтому payload — это
/// `CallToolResult`-обёртка rmcp вида `{"content":[{"type":"text","text":"<JSON>"}]}`,
/// где наш реальный JSON `{result, _meta}` лежит внутри `content[0].text`.
/// На случай non-MCP бэкендов и совместимости — пробуем сначала верхний уровень,
/// потом распаковываем вложенный JSON-string внутри content[0].text.
///
/// Ожидаемые формы:
/// 1) Top-level (non-MCP бэкенд):
///    ```json
///    {"result": [...], "_meta": {"dependent_files": ["src/X.bsl"]}}
///    ```
/// 2) Вложенная (MCP CallToolResult от rmcp):
///    ```json
///    {"content":[{"type":"text","text":"{\"result\":...,\"_meta\":{\"dependent_files\":[...]}}"}]}
///    ```
fn extract_dependent_files(payload: &str) -> Vec<String> {
    let parsed: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Вариант 1: top-level `_meta.dependent_files` (не-MCP бэкенды).
    let top_level = parsed
        .get("_meta")
        .and_then(|m| m.get("dependent_files"))
        .and_then(|fs| fs.as_array());
    if let Some(arr) = top_level {
        return arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }

    // Вариант 2: MCP CallToolResult. Распакуем content[0].text как вложенный
    // JSON, поищем `_meta.dependent_files` там. Если content имеет несколько
    // text-частей — берём первую (на практике в наших tools всегда одна).
    let nested_text = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.iter().find(|item| item.get("type").and_then(|t| t.as_str()) == Some("text")))
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str());
    if let Some(text) = nested_text {
        if let Ok(inner) = serde_json::from_str::<Value>(text) {
            return inner
                .get("_meta")
                .and_then(|m| m.get("dependent_files"))
                .and_then(|fs| fs.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
        }
    }

    Vec::new()
}

/// Извлечь `_meta.file_mtimes` (карта `rel_path → индексный mtime`, unix-секунды)
/// из ответа serve. Зеркало [`extract_dependent_files`]: пробует top-level, затем
/// MCP-обёртку `content[0].text`. Отсутствие/невалидность → пустая карта. Вход
/// для write-triggered ленивой ревалидации (#1471).
fn extract_file_mtimes(payload: &str) -> std::collections::HashMap<String, i64> {
    use std::collections::HashMap;
    fn from_meta(v: &Value) -> Option<HashMap<String, i64>> {
        let obj = v.get("_meta")?.get("file_mtimes")?.as_object()?;
        Some(
            obj.iter()
                .filter_map(|(k, val)| val.as_i64().map(|m| (k.clone(), m)))
                .collect(),
        )
    }
    let parsed: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    if let Some(m) = from_meta(&parsed) {
        return m;
    }
    let nested_text = parsed
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
        })
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str());
    if let Some(text) = nested_text {
        if let Ok(inner) = serde_json::from_str::<Value>(text) {
            if let Some(m) = from_meta(&inner) {
                return m;
            }
        }
    }
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingBackend {
        calls: AtomicUsize,
        response: String,
    }

    #[async_trait::async_trait]
    impl BackendCaller for CountingBackend {
        async fn call(&self, _tool: &str, _args: &Value) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.response.clone())
        }
    }

    fn build_proxy(policy: Policy, response: &str) -> (CacheProxy<CountingBackend>, Arc<CountingBackend>) {
        let backend = Arc::new(CountingBackend {
            calls: AtomicUsize::new(0),
            response: response.to_string(),
        });
        let proxy = CacheProxy::new(
            "ci",
            vec!["repo".to_string()],
            Arc::new(arc_swap::ArcSwap::from_pointee(policy)),
            Arc::new(Cache::new()),
            Arc::new(SingleFlight::new()),
            Arc::new(Metrics::new()),
            backend.clone(),
            crate::freeze::FreezeController::new(),
            Arc::new(DirtySet::new()),
            RevalConfig::default(),
        );
        (proxy, backend)
    }

    /// Backend, отдающий ответы по индексу вызова (последний повторяется) —
    /// моделирует «индекс ещё не догнал → догнал» между ретраями ревалидации.
    struct SeqBackend {
        calls: AtomicUsize,
        responses: Vec<String>,
    }

    #[async_trait::async_trait]
    impl BackendCaller for SeqBackend {
        async fn call(&self, _tool: &str, _args: &Value) -> Result<String, String> {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            let idx = i.min(self.responses.len() - 1);
            Ok(self.responses[idx].clone())
        }
    }

    fn build_proxy_reval(
        responses: Vec<&str>,
        max_wait_ms: u64,
    ) -> (CacheProxy<SeqBackend>, Arc<SeqBackend>, Arc<DirtySet>) {
        let backend = Arc::new(SeqBackend {
            calls: AtomicUsize::new(0),
            responses: responses.into_iter().map(String::from).collect(),
        });
        let dirty = Arc::new(DirtySet::new());
        let reval = RevalConfig {
            enabled: true,
            max_wait: Duration::from_millis(max_wait_ms),
            retry_interval: Duration::from_millis(5),
        };
        let proxy = CacheProxy::new(
            "ci",
            vec!["repo".to_string()],
            Arc::new(arc_swap::ArcSwap::from_pointee(Policy::default())),
            Arc::new(Cache::new()),
            Arc::new(SingleFlight::new()),
            Arc::new(Metrics::new()),
            backend.clone(),
            crate::freeze::FreezeController::new(),
            dirty.clone(),
            reval,
        );
        (proxy, backend, dirty)
    }

    const R_BEHIND: &str = r#"{"result":[],"_meta":{"dependent_files":["src/X.bsl"],"file_mtimes":{"src/X.bsl":90}}}"#;
    const R_CAUGHT: &str = r#"{"result":[],"_meta":{"dependent_files":["src/X.bsl"],"file_mtimes":{"src/X.bsl":100}}}"#;

    #[tokio::test]
    async fn revalidation_caches_only_when_index_caught_up() {
        // Раунд 1 — индекс отстаёт (90<100), раунд 2 — догнал (100>=100).
        let (proxy, backend, dirty) = build_proxy_reval(vec![R_BEHIND, R_CAUGHT], 1000);
        dirty.mark("ut", "src/X.bsl", 100);
        let args = json!({"repo": "ut", "query": "F"});

        let _ = proxy.handle("search_function", &args, false).await.unwrap();
        // Два форварда: первый отстал, второй догнал и закэшировался.
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(proxy.cache.len(), 1, "ответ закэширован после догона");
        assert_eq!(
            dirty.observed("ut", "src/X.bsl"),
            None,
            "флаг снят после догона"
        );

        // Повтор — чистый HIT, в backend не ходим.
        let _ = proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_hits, 1);
    }

    #[tokio::test]
    async fn eventual_serves_stale_without_caching_when_behind() {
        // max_wait=0 → eventual: один форвард, индекс отстаёт → отдать без кэша.
        let (proxy, backend, dirty) = build_proxy_reval(vec![R_BEHIND], 0);
        dirty.mark("ut", "src/X.bsl", 100);
        let args = json!({"repo": "ut", "query": "F"});

        let _ = proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1, "ровно один форвард");
        assert_eq!(proxy.cache.len(), 0, "старьё в кэш не попало");
        assert_eq!(
            dirty.observed("ut", "src/X.bsl"),
            Some(100),
            "флаг остаётся грязным"
        );
    }

    #[tokio::test]
    async fn dirty_hit_triggers_revalidation_not_stale_serve() {
        // Backend всегда отдаёт догнавший ответ. Сначала кладём в кэш (чисто),
        // потом метим грязным → следующий HIT должен пойти на ревалидацию.
        let (proxy, backend, dirty) = build_proxy_reval(vec![R_CAUGHT], 1000);
        let args = json!({"repo": "ut", "query": "F"});

        // Чистый MISS → закэшировали (грязных файлов нет → all_caught=true).
        let _ = proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(proxy.cache.len(), 1);

        // Метим грязным с observed, который индекс уже перекрывает (100>=100).
        dirty.mark("ut", "src/X.bsl", 100);
        // dirty-HIT → форвард (а не отдача из кэша), индекс догнал → re-cache+clear.
        let _ = proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            2,
            "dirty-hit обязан сходить в backend"
        );
        assert_eq!(dirty.observed("ut", "src/X.bsl"), None, "флаг снят");
    }

    #[test]
    fn extract_file_mtimes_top_level() {
        let m = super::extract_file_mtimes(R_CAUGHT);
        assert_eq!(m.get("src/X.bsl"), Some(&100));
    }

    #[test]
    fn extract_file_mtimes_mcp_wrapper() {
        let inner = R_CAUGHT;
        let inner_escaped = serde_json::to_string(inner).unwrap();
        let payload = format!(r#"{{"content":[{{"type":"text","text":{}}}]}}"#, inner_escaped);
        let m = super::extract_file_mtimes(&payload);
        assert_eq!(m.get("src/X.bsl"), Some(&100));
    }

    #[test]
    fn extract_file_mtimes_absent_is_empty() {
        assert!(super::extract_file_mtimes(r#"{"result":[]}"#).is_empty());
        assert!(super::extract_file_mtimes("not json").is_empty());
    }

    #[tokio::test]
    async fn second_call_is_cache_hit() {
        let (proxy, backend) = build_proxy(Policy::default(), r#"{"ok":true}"#);
        let args = json!({"x": 1});

        let r1 = proxy.handle("search_function", &args, false).await.unwrap();
        let r2 = proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(*r1, *r2);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_hits, 1);
        assert_eq!(snap.cache_misses, 1);
    }

    #[tokio::test]
    async fn non_cacheable_tool_bypasses() {
        let mut policy = Policy::default();
        policy.tools.insert(
            "execute_query".into(),
            crate::policy::ToolPolicy {
                cacheable: false,
                ttl_seconds: None,
            },
        );
        let (proxy, backend) = build_proxy(policy, r#"[]"#);
        let args = json!({"query": "ВЫБРАТЬ 1"});

        proxy.handle("execute_query", &args, false).await.unwrap();
        proxy.handle("execute_query", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.bypass, 2);
        assert_eq!(snap.cache_hits, 0);
    }

    #[tokio::test]
    async fn explicit_bypass_skips_cache() {
        let (proxy, backend) = build_proxy(Policy::default(), r#"{"v":1}"#);
        let args = json!({"a": 1});
        proxy.handle("search_function", &args, false).await.unwrap();
        proxy.handle("search_function", &args, true).await.unwrap(); // bypass
        proxy.handle("search_function", &args, false).await.unwrap(); // снова cache hit
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_hits, 1);
        assert_eq!(snap.bypass, 1);
    }

    #[tokio::test]
    async fn frozen_scope_returns_error_without_backend_call() {
        let (proxy, backend) = build_proxy(Policy::default(), r#"{"v":1}"#);
        proxy.freeze.freeze("ut", std::time::Duration::from_secs(60));

        let args_ut = json!({"repo": "ut", "query": "X"});
        let res = proxy.handle("search_function", &args_ut, false).await;
        match res {
            Err(ProxyError::Frozen { scope, .. }) => assert_eq!(scope, "ut"),
            other => panic!("ожидали Frozen, получили {other:?}"),
        }
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            0,
            "frozen scope не должен ходить в backend"
        );

        // Другой scope должен работать.
        let args_bp = json!({"repo": "bp-ss", "query": "X"});
        proxy.handle("search_function", &args_bp, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn thaw_restores_traffic_to_backend() {
        let (proxy, backend) = build_proxy(Policy::default(), r#"{"v":1}"#);
        proxy.freeze.freeze("ut", std::time::Duration::from_secs(60));

        let args = json!({"repo": "ut", "query": "X"});
        assert!(proxy.handle("search_function", &args, false).await.is_err());

        proxy.freeze.thaw("ut");
        proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn keys_for_different_repos_are_independent_in_cache() {
        let (proxy, backend) = build_proxy(Policy::default(), r#"{"v":1}"#);
        let a_ut = json!({"repo": "ut", "query": "X"});
        let a_bp = json!({"repo": "bp-ss", "query": "X"});

        proxy.handle("search_function", &a_ut, false).await.unwrap();
        proxy.handle("search_function", &a_bp, false).await.unwrap();
        // Разные repo — два разных miss + два backend call.
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        // Повтор того же repo=ut — hit, без обращения в backend.
        proxy.handle("search_function", &a_ut, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_misses, 2);
        assert_eq!(snap.cache_hits, 1);
    }

    #[tokio::test]
    async fn scope_with_cacheable_false_bypasses_cache() {
        let mut policy = Policy::default();
        policy.scopes.insert(
            "ut".to_string(),
            crate::policy::ScopePolicy { cacheable: false },
        );
        let (proxy, backend) = build_proxy(policy, r#"{"v":1}"#);
        let args_ut = json!({"repo": "ut", "query": "X"});

        proxy.handle("search_function", &args_ut, false).await.unwrap();
        proxy.handle("search_function", &args_ut, false).await.unwrap();
        // Оба запроса дошли до backend — нет cache hit.
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_hits, 0);
        assert_eq!(snap.bypass, 2, "оба ушли как bypass из-за scope=ut disabled");
        assert_eq!(snap.cache_size, 0, "ничего не оседает в кэше");
    }

    #[tokio::test]
    async fn scope_disabled_does_not_affect_other_scopes() {
        let mut policy = Policy::default();
        policy.scopes.insert(
            "ut".to_string(),
            crate::policy::ScopePolicy { cacheable: false },
        );
        let (proxy, backend) = build_proxy(policy, r#"{"v":1}"#);

        // ut — bypass, оба запроса в backend.
        let args_ut = json!({"repo": "ut", "query": "X"});
        proxy.handle("search_function", &args_ut, false).await.unwrap();
        proxy.handle("search_function", &args_ut, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        // bp-ss — нормальное кэширование, второй запрос cache hit.
        let args_bp = json!({"repo": "bp-ss", "query": "X"});
        proxy.handle("search_function", &args_bp, false).await.unwrap();
        proxy.handle("search_function", &args_bp, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3, "bp-ss: 1 miss + 1 hit");

        let snap = proxy.metrics.snapshot();
        assert_eq!(snap.cache_hits, 1);
        assert_eq!(snap.bypass, 2);
    }

    #[tokio::test]
    async fn meta_dependent_files_populates_reverse_index() {
        let payload_with_meta = r#"{"result":[{"name":"F","file_path":"src/X.bsl"}],"_meta":{"dependent_files":["src/X.bsl","src/Y.bsl"]}}"#;
        let (proxy, _backend) = build_proxy(Policy::default(), payload_with_meta);
        let args = json!({"repo": "ut", "query": "F"});

        proxy.handle("search_function", &args, false).await.unwrap();
        assert_eq!(proxy.cache.reverse_index_size(), 2);

        // Точечная инвалидация по src/X.bsl сносит entry, по src/Z.bsl — нет.
        let removed = proxy.cache.invalidate_files(&["src/X.bsl".to_string()]);
        assert_eq!(removed, 1);
        assert_eq!(proxy.cache.len(), 0);
    }

    #[tokio::test]
    async fn meta_absent_falls_back_to_plain_insert() {
        let payload_no_meta = r#"{"result":[{"name":"F","file_path":"src/X.bsl"}]}"#;
        let (proxy, _backend) = build_proxy(Policy::default(), payload_no_meta);
        let args = json!({"repo": "ut", "query": "F"});

        proxy.handle("search_function", &args, false).await.unwrap();
        // Reverse_index пуст — бэкенд не прислал dependent_files, обратная совместимость.
        assert_eq!(proxy.cache.reverse_index_size(), 0);
        assert_eq!(proxy.cache.len(), 1);
    }

    #[test]
    fn extract_dependent_files_extracts_paths() {
        let payload = r#"{"result":[],"_meta":{"dependent_files":["a.bsl","b.bsl"]}}"#;
        let deps = super::extract_dependent_files(payload);
        assert_eq!(deps, vec!["a.bsl".to_string(), "b.bsl".to_string()]);
    }

    #[test]
    fn extract_dependent_files_handles_missing_meta() {
        let payload = r#"{"result":[]}"#;
        assert!(super::extract_dependent_files(payload).is_empty());
    }

    #[test]
    fn extract_dependent_files_handles_invalid_json() {
        let payload = "not a json";
        assert!(super::extract_dependent_files(payload).is_empty());
    }

    #[test]
    fn extract_dependent_files_handles_non_array() {
        let payload = r#"{"_meta":{"dependent_files":"not an array"}}"#;
        assert!(super::extract_dependent_files(payload).is_empty());
    }

    #[test]
    fn extract_dependent_files_unpacks_mcp_call_tool_result() {
        // Реальная форма ответа MCP rmcp tools/call: наш JSON завёрнут
        // в CallToolResult с content array.
        let inner = r#"{"result":[],"_meta":{"dependent_files":["src/X.bsl","src/Y.bsl"]}}"#;
        let inner_escaped = serde_json::to_string(inner).unwrap();
        let payload = format!(
            r#"{{"content":[{{"type":"text","text":{}}}]}}"#,
            inner_escaped
        );
        let deps = super::extract_dependent_files(&payload);
        assert_eq!(
            deps,
            vec!["src/X.bsl".to_string(), "src/Y.bsl".to_string()]
        );
    }

    #[test]
    fn extract_dependent_files_handles_mcp_wrapper_without_meta() {
        // CallToolResult с content, но без _meta внутри — пустой ответ.
        let inner = r#"{"result":[]}"#;
        let inner_escaped = serde_json::to_string(inner).unwrap();
        let payload = format!(
            r#"{{"content":[{{"type":"text","text":{}}}]}}"#,
            inner_escaped
        );
        assert!(super::extract_dependent_files(&payload).is_empty());
    }

    #[test]
    fn strip_meta_removes_meta_from_mcp_wrapper() {
        let inner = r#"{"result":[1,2],"_meta":{"dependent_files":["a.bsl"],"file_mtimes":{"a.bsl":5}}}"#;
        let inner_escaped = serde_json::to_string(inner).unwrap();
        let payload = format!(r#"{{"content":[{{"type":"text","text":{}}}]}}"#, inner_escaped);
        // deps извлекаются ДО strip — инвалидация не страдает.
        assert_eq!(super::extract_dependent_files(&payload), vec!["a.bsl".to_string()]);
        let clean = super::strip_meta(&payload);
        assert!(!clean.contains("_meta"), "clean содержит _meta: {}", clean);
        assert!(!clean.contains("file_mtimes"));
        assert!(!clean.contains("dependent_files"));
        // полезная нагрузка result сохранена внутри content[0].text.
        assert!(clean.contains("result"), "result потерян: {}", clean);
    }

    #[test]
    fn strip_meta_removes_top_level_meta() {
        let payload = r#"{"result":[],"_meta":{"dependent_files":["x"]}}"#;
        let clean = super::strip_meta(payload);
        assert!(!clean.contains("_meta"));
        assert!(clean.contains("result"));
    }

    #[test]
    fn strip_meta_removes_meta_from_structured_content() {
        // WS-3: реальная форма ответа extension-tools serve (rmcp structured
        // output) — `_meta` живёт и в content[0].text, и в structuredContent.
        // До фикса чистился только text, structuredContent доезжал до клиента.
        let inner = r#"{"result":{"name":"X"},"_meta":{"dependent_files":["a.bsl"]}}"#;
        let inner_escaped = serde_json::to_string(inner).unwrap();
        let payload = format!(
            r#"{{"content":[{{"type":"text","text":{}}}],"structuredContent":{{"_meta":{{"dependent_files":["a.bsl"]}},"result":{{"name":"X"}}}},"isError":false}}"#,
            inner_escaped
        );
        // deps извлекаются ДО strip — инвалидация не страдает.
        assert_eq!(
            super::extract_dependent_files(&payload),
            vec!["a.bsl".to_string()]
        );
        let clean = super::strip_meta(&payload);
        assert!(!clean.contains("_meta"), "clean содержит _meta: {}", clean);
        assert!(!clean.contains("dependent_files"));
        // Полезная нагрузка обоих каналов цела.
        let v: Value = serde_json::from_str(&clean).unwrap();
        assert_eq!(v["structuredContent"]["result"]["name"], "X");
        let inner_clean: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(inner_clean["result"]["name"], "X");
    }

    #[test]
    fn strip_meta_noop_without_meta() {
        let payload = r#"{"result":[1,2,3]}"#;
        assert_eq!(super::strip_meta(payload), payload.to_string());
    }

    #[test]
    fn strip_meta_noop_on_invalid_json() {
        assert_eq!(super::strip_meta("not json"), "not json".to_string());
    }
}
