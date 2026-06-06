//! Общее ядро кэширующего MCP-прокси.
//!
//! Состав:
//! - [`config`]   — структуры конфига и парсинг TOML.
//! - [`policy`]   — политика TTL по имени tool'а + per-scope cacheable.
//! - [`cache`]    — in-memory хранилище ответов на DashMap с TTL + reverse_index.
//! - [`reverse_index`] — `file_path → cache_keys` для точечной инвалидации.
//! - [`freeze`]   — block-режим: scope → instant, snapshot через ArcSwap.
//! - [`singleflight`] — координация одинаковых in-flight запросов.
//! - [`metrics`]  — счётчики hit/miss и среднего latency бэкенда.
//! - [`proxy`]    — главный обработчик: оркестрирует кэш + single-flight + бэкенд.
//! - [`rmcp_client`] — реализация [`BackendCaller`] через rmcp (форвард на бэкенд).
//! - [`server`]   — `ServerHandler` прокси (приём запросов от клиента).

pub mod cache;
pub mod config;
pub mod dirty;
pub mod freeze;
pub mod metrics;
pub mod policy;
pub mod proxy;
pub mod reverse_index;
pub mod rmcp_client;
pub mod server;
pub mod singleflight;

pub use cache::{Cache, CacheEntry};
pub use config::{BackendConfig, CacheConfig, HealthConfig, ProxyConfig, ServerConfig};
pub use dirty::DirtySet;
pub use freeze::{FreezeController, FreezeEntry, FreezeState};
pub use metrics::{Metrics, MetricsSnapshot};
pub use policy::{Policy, ScopePolicy, ToolPolicy};
pub use proxy::{BackendCaller, CacheProxy, ProxyError, ProxyResult, RevalConfig};
pub use reverse_index::ReverseIndex;
pub use rmcp_client::{RmcpBackend, SharedBackend};
pub use server::ProxyServer;
pub use singleflight::SingleFlight;
