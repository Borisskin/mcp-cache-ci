//! Структуры конфига и парсинг TOML.
//!
//! Иерархия источников (от низшего приоритета к высшему):
//! 1. дефолты в коде (через `Default`),
//! 2. базовый файл `cache.toml`,
//! 3. ENV-override (`MCP_CACHE_*`) — на стороне бинарника, не здесь,
//! 4. CLI-флаги — там же.
//!
//! Никаких хардкодов IP/портов/TTL: всё значимое читается из файла.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Корневой конфиг прокси. Точка входа парсинга TOML-файла.
///
/// `deny_unknown_fields` ловит частую TOML-ошибку: ключ верхнего уровня
/// (`policy_path`) поставлен ПОСЛЕ первой `[секции]` и парсер считает его
/// частью этой секции. С `deny_unknown_fields` неизвестное поле вылетает
/// в ошибке парсинга вместо тихого "пропало".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub backend: BackendConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub health: HealthConfig,
    /// Путь к файлу `cache_policy.toml` относительно каталога процесса
    /// или абсолютный. Если не указан — используются дефолты `Policy::default()`.
    pub policy_path: Option<String>,
}

impl ProxyConfig {
    /// Прочитать TOML-файл и распарсить в `ProxyConfig`.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать конфиг: {}", path.display()))?;
        let cfg: ProxyConfig = toml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать TOML: {}", path.display()))?;
        Ok(cfg)
    }
}

/// Параметры HTTP-сервера прокси (наш биндинг).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Только loopback или внутренний интерфейс. По правилу `mcp-deploy-procedure.md`
    /// никаких `0.0.0.0` — кроме случаев когда прокси слушает на ВМ внутри
    /// доверенной локальной сети.
    pub bind_host: String,
    pub bind_port: u16,
    /// Список разрешённых Host-заголовков (для DNS rebinding защиты в rmcp 1.5+).
    /// Если None — используются дефолты rmcp `["localhost", "127.0.0.1", "::1"]`.
    /// Для прокси на ВМ, к которому ходят с других машин, нужно явно
    /// добавить IP/имена этих машин (например `["203.0.113.10", "192.0.2.50"]`).
    pub allowed_hosts: Option<Vec<String>>,
}

/// Параметры обращения к бэкенду (тот MCP-сервер, который мы кэшируем).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Полный URL `streamable-http` MCP-эндпоинта бэкенда.
    /// Пример: `http://127.0.0.1:8011/mcp`.
    pub url: String,
    /// Таймаут одного round-trip к бэкенду, мс.
    #[serde(default = "default_backend_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_backend_timeout_ms() -> u64 {
    5000
}

/// Параметры кэша.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Максимум записей в кэше. При превышении — старые вытесняются по TTL,
    /// размер-ограниченная LRU-эвикция в задачах второго этапа (`moka`).
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
    /// Грубый верхний предел RAM на одного хранителя кэша, МБ. Используется
    /// в самодиагностике и health-эндпоинте, не enforced на этапе 1.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: usize,
    /// TTL по умолчанию (если конкретный tool не упомянут в политике), сек.
    #[serde(default = "default_ttl_seconds")]
    pub default_ttl_seconds: u64,
    /// Период фоновой эвикции протухших записей, сек. 0 — отключить.
    #[serde(default = "default_evict_interval_seconds")]
    pub evict_interval_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: default_max_entries(),
            max_memory_mb: default_max_memory_mb(),
            default_ttl_seconds: default_ttl_seconds(),
            evict_interval_seconds: default_evict_interval_seconds(),
        }
    }
}

fn default_max_entries() -> usize {
    10_000
}
fn default_max_memory_mb() -> usize {
    200
}
fn default_ttl_seconds() -> u64 {
    600
}
fn default_evict_interval_seconds() -> u64 {
    60
}

/// Параметры health-чека бэкенда.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Период автоматических ping'ов бэкенда (для `/health`-кэша), сек.
    #[serde(default = "default_health_interval_s")]
    pub interval_s: u64,
    /// Таймаут одного health-ping'а, мс.
    #[serde(default = "default_health_timeout_ms")]
    pub backend_timeout_ms: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            interval_s: default_health_interval_s(),
            backend_timeout_ms: default_health_timeout_ms(),
        }
    }
}

fn default_health_interval_s() -> u64 {
    30
}
fn default_health_timeout_ms() -> u64 {
    2000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let toml = r#"
            [server]
            bind_host = "127.0.0.1"
            bind_port = 8013

            [backend]
            url = "http://127.0.0.1:8011/mcp"
        "#;
        let cfg: ProxyConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.server.bind_port, 8013);
        assert_eq!(cfg.backend.url, "http://127.0.0.1:8011/mcp");
        assert_eq!(cfg.backend.timeout_ms, 5000);
        assert_eq!(cfg.cache.default_ttl_seconds, 600);
        assert_eq!(cfg.health.interval_s, 30);
        assert!(cfg.policy_path.is_none());
    }
}
