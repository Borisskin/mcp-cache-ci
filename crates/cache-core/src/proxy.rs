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

/// Кэш-прокси: связывает кэш, политику, single-flight и backend-caller.
///
/// `scope_arg_name` — имя поля в args, по которому строится scope-prefix кэш-ключа
/// и проверяется заморозка. Для `mcp-cache-ci` это `"repo"`. Если поле в args
/// отсутствует — scope пустой (`""`), инвалидация по `repo` такие записи не
/// задевает (но `all:true` снесёт).
pub struct CacheProxy<B: BackendCaller> {
    pub server_alias: String,
    pub scope_arg_name: Option<String>,
    pub policy: Arc<arc_swap::ArcSwap<Policy>>,
    pub cache: Arc<Cache>,
    pub singleflight: Arc<SingleFlight<String>>,
    pub metrics: Arc<Metrics>,
    pub backend: Arc<B>,
    pub freeze: FreezeController,
}

impl<B: BackendCaller> CacheProxy<B> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server_alias: impl Into<String>,
        scope_arg_name: Option<impl Into<String>>,
        policy: Arc<arc_swap::ArcSwap<Policy>>,
        cache: Arc<Cache>,
        singleflight: Arc<SingleFlight<String>>,
        metrics: Arc<Metrics>,
        backend: Arc<B>,
        freeze: FreezeController,
    ) -> Self {
        Self {
            server_alias: server_alias.into(),
            scope_arg_name: scope_arg_name.map(Into::into),
            policy,
            cache,
            singleflight,
            metrics,
            backend,
            freeze,
        }
    }

    /// Извлечь scope из args по `scope_arg_name`. Если `scope_arg_name` не задан
    /// или поле отсутствует/не строка — вернуть пустую строку.
    fn extract_scope(&self, args: &Value) -> String {
        let Some(name) = &self.scope_arg_name else {
            return String::new();
        };
        args.get(name)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
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

        // Hit?
        if let Some(payload) = self.cache.get(&key) {
            self.metrics.record_hit();
            debug!(tool = %tool, "cache hit");
            return Ok(payload);
        }

        // Miss → single-flight в бэкенд.
        self.metrics.record_miss();
        let ttl = Duration::from_secs(ttl.unwrap_or(0).max(1));
        let cache = self.cache.clone();
        let metrics = self.metrics.clone();
        let key_for_cache = key.clone();

        // Клон ссылок специально под closure single-flight'а: эти переменные
        // уйдут внутрь, а внешние `cache`/`metrics` останутся для пост-обработки.
        let work = {
            let backend = self.backend.clone();
            let metrics_inner = metrics.clone();
            let tool_owned = tool.to_string();
            let args_owned = args.clone();
            move || async move {
                let started = Instant::now();
                let payload = backend.call(&tool_owned, &args_owned).await?;
                metrics_inner
                    .observe_backend_latency_micros(started.elapsed().as_micros() as u64);
                Ok::<String, String>(payload)
            }
        };

        let result = self.singleflight.do_or_join(key, work).await;

        match result {
            Ok(shared_payload) => {
                // shared_payload: Arc<String> уже из SingleFlight.
                // Парсим payload: если бэкенд вернул `_meta.dependent_files`,
                // регистрируем эти связи в reverse_index через insert_with_deps.
                // Иначе — обычный insert без зависимостей (обратная совместимость
                // со старыми бэкендами или tool-вызовами без metadata).
                let deps = extract_dependent_files(&shared_payload);
                if deps.is_empty() {
                    cache.insert(key_for_cache, shared_payload.clone(), ttl);
                } else {
                    cache.insert_with_deps(
                        key_for_cache,
                        shared_payload.clone(),
                        ttl,
                        deps,
                    );
                }
                metrics.update_cache_size(cache.len());
                Ok(shared_payload)
            }
            Err(err) => {
                metrics.record_backend_error();
                warn!(tool = %tool, error = %err, "backend call failed");
                Err(ProxyError::Backend(err))
            }
        }
    }

    async fn forward_no_cache(&self, tool: &str, args: &Value) -> ProxyResult {
        let started = Instant::now();
        match self.backend.call(tool, args).await {
            Ok(payload) => {
                self.metrics
                    .observe_backend_latency_micros(started.elapsed().as_micros() as u64);
                Ok(Arc::new(payload))
            }
            Err(err) => {
                self.metrics.record_backend_error();
                Err(ProxyError::Backend(err))
            }
        }
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
            Some("repo"),
            Arc::new(arc_swap::ArcSwap::from_pointee(policy)),
            Arc::new(Cache::new()),
            Arc::new(SingleFlight::new()),
            Arc::new(Metrics::new()),
            backend.clone(),
            crate::freeze::FreezeController::new(),
        );
        (proxy, backend)
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
}
