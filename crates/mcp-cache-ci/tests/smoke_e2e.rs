//! E2E smoke-тест: реальный rmcp-клиент стучится на прокси, прокси форвардит
//! на code-index (`http://127.0.0.1:8011/mcp`), кэширует, отдаёт.
//!
//! Тест помечен `#[ignore]` — он требует:
//! - `code-index serve` на 8011 (живой);
//! - `mcp-cache-ci` на 8013 (живой, запущенный с `config/cache-ci.toml`).
//!
//! Запуск:
//! ```
//! cargo test --test smoke_e2e -- --ignored --nocapture
//! ```

use rmcp::{
    model::CallToolRequestParams, transport::StreamableHttpClientTransport, ServiceExt,
};
use serde_json::Value;

const PROXY_URL: &str = "http://127.0.0.1:8013/mcp";
const METRICS_URL: &str = "http://127.0.0.1:8013/metrics";

#[tokio::test]
#[ignore]
async fn smoke_e2e_proxy_caches_get_stats() {
    // 1) Подключаемся к прокси как MCP-клиент.
    let transport = StreamableHttpClientTransport::from_uri(PROXY_URL);
    let client = ()
        .serve(transport)
        .await
        .expect("rmcp client must connect to proxy /mcp");

    // 2) tools/list — проверяем что прокси отдаёт набор от code-index.
    let tools = client
        .list_all_tools()
        .await
        .expect("list_all_tools must succeed");
    assert!(
        !tools.is_empty(),
        "ожидали непустой tools/list, прокси что-то сломал"
    );
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    eprintln!("tools от прокси ({}): {:?}", names.len(), names);
    assert!(
        names.iter().any(|n| *n == "get_stats"),
        "ожидали что среди tool'ов есть get_stats, нашли: {:?}",
        names
    );

    // 3) Снимок метрик ДО.
    let m_before = fetch_metrics().await;
    eprintln!("metrics до: {}", m_before);

    // 4) Первый вызов get_stats — должен быть cache miss.
    let result1 = client
        .peer()
        .call_tool(CallToolRequestParams::new("get_stats"))
        .await
        .expect("first call_tool(get_stats) must succeed");
    assert!(
        result1.is_error.is_none() || result1.is_error == Some(false),
        "первый вызов вернул is_error: {:?}",
        result1
    );
    eprintln!("первый ответ: {} content blocks", result1.content.len());

    // 5) Второй вызов с теми же args — должен быть cache hit.
    let result2 = client
        .peer()
        .call_tool(CallToolRequestParams::new("get_stats"))
        .await
        .expect("second call_tool(get_stats) must succeed");
    eprintln!("второй ответ: {} content blocks", result2.content.len());

    // 6) Снимок метрик ПОСЛЕ.
    let m_after = fetch_metrics().await;
    eprintln!("metrics после: {}", m_after);

    let hits_before = m_before["cache_hits"].as_u64().unwrap_or(0);
    let misses_before = m_before["cache_misses"].as_u64().unwrap_or(0);
    let hits_after = m_after["cache_hits"].as_u64().unwrap_or(0);
    let misses_after = m_after["cache_misses"].as_u64().unwrap_or(0);

    assert!(
        misses_after - misses_before >= 1,
        "ожидали хотя бы 1 cache miss за тест, было {} → стало {}",
        misses_before,
        misses_after
    );
    assert!(
        hits_after - hits_before >= 1,
        "ожидали хотя бы 1 cache hit за тест, было {} → стало {}",
        hits_before,
        hits_after
    );

    // 7) Третий вызов с другими args (repo=ut) — снова miss.
    let mut params_ut = CallToolRequestParams::new("get_stats");
    let mut args = serde_json::Map::new();
    args.insert("repo".into(), Value::String("ut".into()));
    params_ut.arguments = Some(args);
    let _result3 = client
        .peer()
        .call_tool(params_ut.clone())
        .await
        .expect("call_tool(get_stats, repo=ut) must succeed");
    let _result4 = client
        .peer()
        .call_tool(params_ut)
        .await
        .expect("call_tool(get_stats, repo=ut) [cached] must succeed");

    let m_final = fetch_metrics().await;
    eprintln!("metrics финал: {}", m_final);
    let hits_final = m_final["cache_hits"].as_u64().unwrap_or(0);
    let misses_final = m_final["cache_misses"].as_u64().unwrap_or(0);

    assert!(
        misses_final - misses_after >= 1,
        "третий вызов с другим args должен был быть miss"
    );
    assert!(
        hits_final - hits_after >= 1,
        "четвёртый вызов с тем же args должен был быть hit"
    );

    eprintln!("✓ smoke pass: hits {} → {}, misses {} → {}",
        hits_before, hits_final, misses_before, misses_final);

    // Корректное завершение клиента.
    let _ = client.cancel().await;
}

async fn fetch_metrics() -> Value {
    reqwest::get(METRICS_URL)
        .await
        .expect("GET /metrics")
        .json::<Value>()
        .await
        .expect("metrics JSON parse")
}
