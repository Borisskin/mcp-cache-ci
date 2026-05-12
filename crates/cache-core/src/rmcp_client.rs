//! Реализация [`crate::BackendCaller`] через rmcp-клиент.
//!
//! Прокси выступает MCP-клиентом по отношению к бэкенду (`code-index serve`):
//! один долгоживущий [`StreamableHttpClientTransport`], полноценный handshake
//! (initialize → notifications/initialized) делается автоматически в
//! `serve_client`.
//!
//! Для каждого `tools/call` от прокси-сервера зовём
//! `peer.call_tool(...)` — rmcp под капотом мультиплексирует JSON-RPC по
//! одной HTTP-сессии.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rmcp::{
    model::{CallToolRequestParams, ListToolsResult, Tool},
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
    ServiceExt,
};
use serde_json::Value;

use crate::proxy::BackendCaller;

/// Долгоживущий клиент к одному бэкенд MCP-серверу. Внутри —
/// `RunningService<RoleClient, ()>`, которым rmcp обслуживает сессию.
pub struct RmcpBackend {
    url: String,
    client: RunningService<RoleClient, ()>,
}

impl std::fmt::Debug for RmcpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RmcpBackend").field("url", &self.url).finish()
    }
}

impl RmcpBackend {
    /// Подключиться к бэкенду по URL `streamable-http`. Делает MCP-handshake.
    pub async fn connect(url: impl Into<String>) -> Result<Self> {
        let url = url.into();
        tracing::info!(target: "cache_core::rmcp_client", url = %url, "connecting to backend");
        let transport = StreamableHttpClientTransport::from_uri(url.clone());
        // Минимальный ClientHandler — пустой кортеж. Уведомления от сервера
        // нам пока не нужны (resources/prompts не используем).
        let client = ()
            .serve(transport)
            .await
            .with_context(|| format!("rmcp serve_client to {url} failed"))?;
        Ok(Self { url, client })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Снять список tool'ов с бэкенда (в постраничном виде, склеив все страницы).
    pub async fn list_all_tools(&self) -> Result<Vec<Tool>> {
        self.client
            .list_all_tools()
            .await
            .with_context(|| format!("list_all_tools failed for {}", self.url))
    }

    /// Полный ответ tools/list — пригодится если хочется передать клиенту
    /// сырой `ListToolsResult`. Сейчас в проксе используем `list_all_tools`.
    #[allow(dead_code)]
    pub async fn list_tools_page(&self) -> Result<ListToolsResult> {
        self.client
            .peer()
            .list_tools(None)
            .await
            .with_context(|| format!("list_tools failed for {}", self.url))
    }

    /// Корректное завершение клиента (закрыть HTTP-сессию).
    pub async fn shutdown(self) {
        if let Err(e) = self.client.cancel().await {
            tracing::warn!(target: "cache_core::rmcp_client", error = ?e, "rmcp client cancel failed");
        }
    }
}

#[async_trait]
impl BackendCaller for RmcpBackend {
    async fn call(&self, tool: &str, args: &Value) -> Result<String, String> {
        // arguments в MCP-протоколе — Map<String, Value>, не Value-целиком.
        // Принимаем любое Value, но если это объект — отдаём его карту,
        // иначе оборачиваем в `{"value": ...}` (вырожденный случай).
        let arguments = match args {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("value".into(), other.clone());
                Some(m)
            }
        };

        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = arguments;

        let result = self
            .client
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| format!("rmcp call_tool({tool}) failed: {e}"))?;

        // Сериализуем CallToolResult в JSON-строку для кэша. На отдаче
        // клиенту прокси десериализует обратно в CallToolResult.
        serde_json::to_string(&result)
            .map_err(|e| format!("serialize CallToolResult failed: {e}"))
    }
}

/// Удобная обёртка вокруг Arc<RmcpBackend> чтобы можно было хранить и в
/// `CacheProxy<B>`, и в Server-handler без перепаковки.
pub type SharedBackend = Arc<RmcpBackend>;
