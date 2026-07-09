//! Декомпозиция БАТЧЕВЫХ tools/call (mass-mode code-index: get_function /
//! get_class с `names[]`, get_object_structure с `full_names[]`) на одиночные
//! под-вызовы.
//!
//! Зачем: кэш-ключ строится по (tool, args) — батч целиком давал ОДИН ключ, и
//! per-object кэш не работал. Здесь батч режется на под-вызовы «по одному
//! имени», каждый проходит штатный [`CacheProxy::handle`] (кэш, single-flight,
//! freeze, dirty-ревалидация, метрики). Кэш-ключ под-вызова байт-в-байт
//! совпадает с ключом прямого одиночного вызова того же инструмента → кэш
//! двусторонне общий: батч греет одиночные вызовы и наоборот. Partial-hit
//! бесплатно: хиты отдаются из кэша, в бэкенд уходят только промахи —
//! ПАРАЛЛЕЛЬНО (buffered, порядок результатов = порядку имён в запросе).
//!
//! Промахи идут в serve ОДИНОЧНЫМИ вызовами, не под-батчем: одиночный ответ
//! serve несёт `_meta.dependent_files`/`file_mtimes` (reverse-index,
//! event-инвалидация per-object), а батчевый ответ serve срезает `_meta` с
//! элементов — под-батч сломал бы инвалидацию per-object записей.
//!
//! Для не-ci бэкендов (mcp-cache-rag и др.) инструментов из [`BATCH_TOOLS`]
//! нет — модуль не активируется, поведение прокси не меняется.

use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};

use crate::proxy::{BackendCaller, CacheProxy, ProxyError};

/// (tool, plural-ключ, singular-ключ, unwrap_result).
///
/// `unwrap_result` зеркалит формат элементов `{results:[...]}` у serve:
/// - core get_function/get_class (false): элемент = одиночный ответ целиком без
///   `_meta` — `{result: [...], hint?}`;
/// - extension get_object_structure (true): элемент = РАЗВЁРНУТОЕ значение
///   `result` одиночного ответа (serve кладёт в results голые структуры).
const BATCH_TOOLS: &[(&str, &str, &str, bool)] = &[
    ("get_function", "names", "name", false),
    ("get_class", "names", "name", false),
    ("get_object_structure", "full_names", "full_name", true),
];

/// Мягкий cap на одновременные под-вызовы в бэкенд внутри одного батча —
/// чтобы батч на сотню имён не выстреливал сотней одновременных HTTP к serve.
const MAX_CONCURRENT_SUBCALLS: usize = 16;

/// Если (tool, args) — батчевый вызов, вернуть (plural, singular, unwrap_result).
/// Критерий: plural-ключ присутствует в args И является массивом (зеркало
/// приоритета serve: `names[]` важнее `name`).
pub fn detect_batch(tool: &str, args: &Value) -> Option<(&'static str, &'static str, bool)> {
    let &(_, plural, singular, unwrap_result) =
        BATCH_TOOLS.iter().find(|(t, _, _, _)| *t == tool)?;
    if args.get(plural).map(|v| v.is_array()).unwrap_or(false) {
        Some((plural, singular, unwrap_result))
    } else {
        None
    }
}

/// Разложить батч на одиночные под-вызовы через [`CacheProxy::handle`],
/// собрать `{"results":[...]}` строго в исходном порядке имён.
///
/// Пер-элементные ошибки (нестроковый элемент, ошибка бэкенда) — `{error}` на
/// своей позиции, не валят весь батч. Исключение — [`ProxyError::Frozen`]:
/// freeze scope-глобальный, все элементы упали бы одинаково, поэтому
/// пробрасывается как ошибка всего батча (консистентно одиночному вызову).
pub async fn handle_batch<B: BackendCaller>(
    proxy: &CacheProxy<B>,
    tool: &str,
    args: &Value,
    bypass: bool,
) -> Result<String, ProxyError> {
    let Some((plural, singular, unwrap_result)) = detect_batch(tool, args) else {
        return Err(ProxyError::Internal(
            "handle_batch вызван не для батчевого инструмента".into(),
        ));
    };
    let items: Vec<Value> = args
        .get(plural)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(json!({ "results": [] }).to_string());
    }

    // Шаблон args одиночного под-вызова: всё из батча (repo, language,
    // path_glob, …), кроме plural-ключа.
    let mut template = args.as_object().cloned().unwrap_or_default();
    template.remove(plural);
    let template = &template;

    let futs = items.into_iter().map(|it| async move {
        let Some(name) = it.as_str() else {
            return Ok(json!({
                "error": format!("{}: каждый элемент должен быть строкой", plural)
            }));
        };
        let mut sub = template.clone();
        sub.insert(singular.to_string(), Value::String(name.to_string()));
        let sub_args = Value::Object(sub);
        match proxy.handle(tool, &sub_args, bypass).await {
            Ok(payload) => Ok(inner_value(&payload, unwrap_result)),
            Err(e @ ProxyError::Frozen { .. }) => Err(e),
            Err(e) => Ok(json!({ "error": e.to_string() })),
        }
    });
    // buffered: конкуррентно до MAX_CONCURRENT_SUBCALLS, порядок сохраняется.
    let collected: Vec<Result<Value, ProxyError>> = stream::iter(futs)
        .buffered(MAX_CONCURRENT_SUBCALLS)
        .collect()
        .await;

    let mut results = Vec::with_capacity(collected.len());
    for r in collected {
        results.push(r?);
    }
    Ok(json!({ "results": results }).to_string())
}

/// Извлечь полезное значение из payload под-вызова. Payload — сериализованный
/// MCP `CallToolResult` (`{"content":[{"type":"text","text":"<json>"}], …}`)
/// от боевого бэкенда, либо plain JSON (тестовые бэкенды / нестандартный
/// ответ) — зеркало логики `strip_meta`/`extract_dependent_files` в proxy.rs.
/// Остаточный `_meta` снимается — паритет с mass-форматом serve, который
/// срезает `_meta` с элементов `{results:[...]}`.
///
/// `unwrap_result=true` дополнительно разворачивает обёртку `{result: X}` → `X`
/// (формат элементов extension-tools, см. [`BATCH_TOOLS`]). Ответ без ключа
/// `result` (например `{error: ...}`) остаётся как есть.
fn inner_value(payload: &str, unwrap_result: bool) -> Value {
    let parsed: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return Value::String(payload.to_string()),
    };
    let mut v = match parsed
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c0| c0.get("text"))
        .and_then(|t| t.as_str())
    {
        Some(text) => {
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string()))
        }
        None => parsed,
    };
    if let Some(o) = v.as_object_mut() {
        o.remove("_meta");
    }
    if unwrap_result {
        if let Some(inner) = v.get_mut("result") {
            return inner.take();
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::dirty::DirtySet;
    use crate::freeze::FreezeController;
    use crate::metrics::Metrics;
    use crate::policy::Policy;
    use crate::proxy::RevalConfig;
    use crate::singleflight::SingleFlight;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Backend, эхо-отвечающий именем из args — для проверки порядка сборки.
    /// Считает вызовы — для проверки partial-hit (в бэкенд уходят только промахи).
    /// На `full_name` отвечает в форме extension-tool (`{result: {...}, _meta}`),
    /// на `name` — плоско (`{echo}`).
    struct EchoBackend {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BackendCaller for EchoBackend {
        async fn call(&self, _tool: &str, args: &Value) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(fqn) = args.get("full_name").and_then(|v| v.as_str()) {
                return Ok(json!({
                    "result": { "full_name": fqn },
                    "_meta": { "dependent_files": [] }
                })
                .to_string());
            }
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("<none>");
            Ok(json!({ "echo": name }).to_string())
        }
    }

    fn build_proxy() -> (CacheProxy<EchoBackend>, Arc<EchoBackend>, FreezeController) {
        let backend = Arc::new(EchoBackend {
            calls: AtomicUsize::new(0),
        });
        let freeze = FreezeController::new();
        let proxy = CacheProxy::new(
            "ci",
            vec!["repo".to_string()],
            Arc::new(arc_swap::ArcSwap::from_pointee(Policy::default())),
            Arc::new(Cache::new()),
            Arc::new(SingleFlight::new()),
            Arc::new(Metrics::new()),
            backend.clone(),
            freeze.clone(),
            Arc::new(DirtySet::new()),
            RevalConfig::default(),
        );
        (proxy, backend, freeze)
    }

    fn parse_results(s: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(s).unwrap()["results"]
            .as_array()
            .cloned()
            .unwrap()
    }

    #[test]
    fn detect_batch_positive_and_negative() {
        let batch = json!({"repo": "r", "names": ["A", "B"]});
        assert_eq!(
            detect_batch("get_function", &batch),
            Some(("names", "name", false))
        );
        assert_eq!(detect_batch("get_class", &batch), Some(("names", "name", false)));
        let oss = json!({"repo": "r", "full_names": ["Catalog.X"]});
        assert_eq!(
            detect_batch("get_object_structure", &oss),
            Some(("full_names", "full_name", true))
        );
        // одиночный вызов — не батч
        assert_eq!(detect_batch("get_function", &json!({"repo": "r", "name": "A"})), None);
        // plural не массив — не батч
        assert_eq!(detect_batch("get_function", &json!({"repo": "r", "names": "A"})), None);
        // чужой инструмент — не батч
        assert_eq!(detect_batch("search_function", &batch), None);
    }

    /// Формат элементов results для get_object_structure идентичен mass-режиму
    /// serve: голая структура, без обёртки {result: ...}.
    #[tokio::test]
    async fn batch_object_structure_unwraps_result() {
        let (proxy, _backend, _f) = build_proxy();
        let args = json!({"repo": "r", "full_names": ["Catalog.X", "Catalog.Y"]});
        let out = handle_batch(&proxy, "get_object_structure", &args, false)
            .await
            .unwrap();
        let results = parse_results(&out);
        assert_eq!(
            results[0]["full_name"].as_str(),
            Some("Catalog.X"),
            "элемент должен быть голой структурой (unwrap result): {:?}",
            results[0]
        );
        assert_eq!(results[1]["full_name"].as_str(), Some("Catalog.Y"));
        assert!(results[0].get("result").is_none());
        assert!(results[0].get("_meta").is_none());
    }

    #[tokio::test]
    async fn batch_preserves_order() {
        let (proxy, _backend, _f) = build_proxy();
        let args = json!({"repo": "r", "names": ["B", "A", "C"]});
        let out = handle_batch(&proxy, "get_function", &args, false)
            .await
            .unwrap();
        let results = parse_results(&out);
        let echoes: Vec<&str> = results.iter().map(|r| r["echo"].as_str().unwrap()).collect();
        assert_eq!(echoes, vec!["B", "A", "C"], "порядок results = порядку имён");
    }

    #[tokio::test]
    async fn batch_caches_per_object_and_partial_hit() {
        let (proxy, backend, _f) = build_proxy();

        // Батч [A, B] → 2 вызова бэкенда.
        let args = json!({"repo": "r", "names": ["A", "B"]});
        handle_batch(&proxy, "get_function", &args, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);

        // Одиночный вызов A — ключ совпадает с под-вызовом из батча → hit, 0 новых.
        let single = json!({"repo": "r", "name": "A"});
        proxy.handle("get_function", &single, false).await.unwrap();
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            2,
            "одиночный вызов после батча обязан попасть в per-object кэш"
        );

        // Partial-hit: батч [A, B, C] → в бэкенд уходит только C.
        let args2 = json!({"repo": "r", "names": ["A", "B", "C"]});
        let out = handle_batch(&proxy, "get_function", &args2, false).await.unwrap();
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            3,
            "в бэкенд должен уйти только промах C"
        );
        let results = parse_results(&out);
        let echoes: Vec<&str> = results.iter().map(|r| r["echo"].as_str().unwrap()).collect();
        assert_eq!(echoes, vec!["A", "B", "C"]);
    }

    #[tokio::test]
    async fn batch_warms_cache_for_single_and_vice_versa() {
        let (proxy, backend, _f) = build_proxy();
        // Прогрев одиночным вызовом → батч получает hit на этот элемент.
        let single = json!({"repo": "r", "name": "X"});
        proxy.handle("get_function", &single, false).await.unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let args = json!({"repo": "r", "names": ["X", "Y"]});
        handle_batch(&proxy, "get_function", &args, false).await.unwrap();
        assert_eq!(
            backend.calls.load(Ordering::SeqCst),
            2,
            "X из кэша (прогрет одиночным), в бэкенд уходит только Y"
        );
    }

    #[tokio::test]
    async fn batch_non_string_element_gets_error_in_place() {
        let (proxy, backend, _f) = build_proxy();
        let args = json!({"repo": "r", "names": ["A", 42, "B"]});
        let out = handle_batch(&proxy, "get_function", &args, false).await.unwrap();
        let results = parse_results(&out);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["echo"].as_str(), Some("A"));
        assert!(
            results[1]["error"].as_str().unwrap().contains("строкой"),
            "нестроковый элемент → error на своей позиции: {:?}",
            results[1]
        );
        assert_eq!(results[2]["echo"].as_str(), Some("B"));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2, "нестроковый элемент в бэкенд не ходит");
    }

    #[tokio::test]
    async fn batch_empty_list_short_circuits() {
        let (proxy, backend, _f) = build_proxy();
        let args = json!({"repo": "r", "names": []});
        let out = handle_batch(&proxy, "get_function", &args, false).await.unwrap();
        assert!(parse_results(&out).is_empty());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0, "пустой батч в бэкенд не ходит");
    }

    #[tokio::test]
    async fn batch_frozen_scope_fails_whole_batch() {
        let (proxy, _backend, freeze) = build_proxy();
        freeze.freeze("r", Duration::from_secs(60));
        let args = json!({"repo": "r", "names": ["A", "B"]});
        let err = handle_batch(&proxy, "get_function", &args, false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProxyError::Frozen { .. }),
            "freeze scope-глобальный → ошибка всего батча, не пер-элементная"
        );
    }

    #[test]
    fn inner_value_unwraps_mcp_calltoolresult() {
        let payload = r#"{"content":[{"type":"text","text":"{\"result\":[1,2],\"hint\":\"h\"}"}]}"#;
        let v = inner_value(payload, false);
        assert_eq!(v["result"][0].as_i64(), Some(1));
        assert_eq!(v["hint"].as_str(), Some("h"));
    }

    #[test]
    fn inner_value_takes_plain_json_and_strips_meta() {
        let payload = r#"{"result":[],"_meta":{"dependent_files":["x"]}}"#;
        let v = inner_value(payload, false);
        assert!(v.get("_meta").is_none(), "_meta должен быть срезан");
        assert!(v["result"].is_array());
    }

    #[test]
    fn inner_value_non_json_text_kept_as_string() {
        let payload = r#"{"content":[{"type":"text","text":"not json"}]}"#;
        assert_eq!(inner_value(payload, false), Value::String("not json".into()));
    }

    #[test]
    fn inner_value_unwrap_result_unwraps_and_keeps_errors() {
        // extension-форма: {result: X} → X
        let payload = r#"{"result":{"full_name":"Catalog.X"},"_meta":{"dependent_files":[]}}"#;
        let v = inner_value(payload, true);
        assert_eq!(v["full_name"].as_str(), Some("Catalog.X"));
        assert!(v.get("result").is_none());
        // ответ без result (ошибка) — как есть
        let payload = r#"{"error":"boom"}"#;
        let v = inner_value(payload, true);
        assert_eq!(v["error"].as_str(), Some("boom"));
    }
}
