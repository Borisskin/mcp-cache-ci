//! `ServerHandler`-реализация прокси: принимает запросы клиента (Claude Code /
//! другой MCP-клиент), форвардит их через [`crate::CacheProxy`] и возвращает
//! либо кэшированный, либо свежий ответ.
//!
//! Tools берутся из снимка, полученного при старте от бэкенда (через
//! `RmcpBackend::list_all_tools`). Снимок хранится в `ArcSwap`, чтобы при
//! желании в будущем можно было пересоздавать его без рестарта (например,
//! по сигналу webhook).

use std::sync::Arc;

use arc_swap::ArcSwap;
use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    ErrorData as McpError, RoleServer,
};
use serde_json::Value;

use crate::proxy::{CacheProxy, ProxyError};
use crate::rmcp_client::RmcpBackend;

/// Прокси-сервер MCP. Один экземпляр на процесс.
#[derive(Clone)]
pub struct ProxyServer {
    pub server_alias: String,
    pub server_version: String,
    pub proxy: Arc<CacheProxy<RmcpBackend>>,
    pub tools: Arc<ArcSwap<Vec<Tool>>>,
}

impl ProxyServer {
    pub fn new(
        server_alias: impl Into<String>,
        server_version: impl Into<String>,
        proxy: Arc<CacheProxy<RmcpBackend>>,
        tools: Vec<Tool>,
    ) -> Self {
        Self {
            server_alias: server_alias.into(),
            server_version: server_version.into(),
            proxy,
            tools: Arc::new(ArcSwap::from_pointee(tools)),
        }
    }

    /// Заменить снимок tool'ов (на случай webhook'а или ручного триггера).
    pub fn replace_tools(&self, tools: Vec<Tool>) {
        self.tools.store(Arc::new(tools));
    }
}

impl ServerHandler for ProxyServer {
    fn get_info(&self) -> ServerInfo {
        let caps = ServerCapabilities::builder().enable_tools().build();
        ServerInfo::new(caps)
            .with_server_info(Implementation::new(
                format!("mcp-cache-{}", self.server_alias),
                self.server_version.clone(),
            ))
            .with_instructions(format!(
                "Кэширующий прокси перед бэкендом '{}'. Все вызовы tools/call \
                 проходят через TTL-кэш. Заголовок X-Cache-Bypass=1 минует кэш.",
                self.server_alias
            ))
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Не страничим: прокси отдаёт весь снимок одним ответом.
        let tools = self.tools.load();
        Ok(ListToolsResult {
            tools: tools.as_ref().clone(),
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_name = params.name.to_string();
        // Превращаем arguments-map обратно в Value для нашего internal API.
        let args = params
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or(Value::Null);

        // bypass пока не поддержан на уровне rmcp (заголовок не доходит сюда).
        // Включим вместе с middleware на следующем шаге.
        let bypass = false;

        // Батчевые вызовы mass-mode (names[]/full_names[]) режутся на одиночные
        // под-вызовы с per-object кэшем — см. crate::batch. Хиты отдаются из
        // кэша, в бэкенд параллельно уходят только промахи; порядок results =
        // порядку имён. Не-батчевые инструменты — прежний путь без изменений.
        if crate::batch::detect_batch(&tool_name, &args).is_some() {
            return match crate::batch::handle_batch(&self.proxy, &tool_name, &args, bypass).await
            {
                Ok(inner) => {
                    // Собираем CallToolResult той же формы, в которой бэкенд
                    // отдаёт одиночные ответы (content[0].text с JSON внутри).
                    let payload = serde_json::json!({
                        "content": [{ "type": "text", "text": inner }]
                    })
                    .to_string();
                    serde_json::from_str::<CallToolResult>(&payload).map_err(|e| {
                        McpError::internal_error(
                            format!("прокси: не удалось собрать батчевый CallToolResult: {e}"),
                            None,
                        )
                    })
                }
                Err(e) => Err(proxy_error_to_mcp(e)),
            };
        }

        match self.proxy.handle(&tool_name, &args, bypass).await {
            Ok(payload) => {
                // payload — это JSON-сериализованный CallToolResult из бэкенда.
                serde_json::from_str::<CallToolResult>(payload.as_str()).map_err(|e| {
                    McpError::internal_error(
                        format!("прокси: не удалось распарсить кэшированный CallToolResult: {e}"),
                        None,
                    )
                })
            }
            Err(e) => Err(proxy_error_to_mcp(e)),
        }
    }
}

/// Маппинг [`ProxyError`] → MCP-ошибка. Общий для одиночного и батчевого путей.
fn proxy_error_to_mcp(err: ProxyError) -> McpError {
    match err {
        ProxyError::Backend(msg) => {
            McpError::internal_error(format!("backend error: {msg}"), None)
        }
        ProxyError::Internal(msg) => McpError::internal_error(msg, None),
        ProxyError::Frozen {
            scope,
            retry_after_seconds,
        } => {
            // Block-режим: кэш заморожен на этот scope (обычно потому, что
            // в 1С случилось обновление конфигурации, а локальный репо ещё
            // не реиндексирован). Возвращаем явную ошибку с подсказкой
            // сколько ждать — клиент LibreChat / Claude Code должен показать
            // пользователю «обновление в процессе» и повторить позже.
            let scope_label = if scope.is_empty() {
                "global".to_string()
            } else {
                scope.clone()
            };
            let message = format!(
                "прокси заморожен (scope='{scope_label}'): обновление конфигурации в процессе, повторите через {retry_after_seconds} сек.",
            );
            let data = serde_json::json!({
                "frozen": true,
                "scope": scope,
                "retry_after_seconds": retry_after_seconds,
            });
            McpError::internal_error(message, Some(data))
        }
    }
}
