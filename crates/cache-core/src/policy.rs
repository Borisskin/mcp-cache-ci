//! Политика TTL по имени MCP-tool'а.
//!
//! Файл политики (`cache_policy.toml`) — отдельный от основного `cache.toml`,
//! чтобы при доработке tool-набора у бэкендов не приходилось трогать
//! параметры HTTP-сервера или кэша.
//!
//! Формат:
//! ```toml
//! default_ttl_seconds = 600
//!
//! [tools."search_function"]
//! ttl_seconds = 3600
//!
//! [tools."execute_query"]
//! cacheable = false
//! ```
//!
//! Имя tool'а в файле указывается **без** MCP-префикса вида `mcp__ci__`,
//! потому что прокси видит «голое» имя из tools/call.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Полная политика по всем tool'ам бэкенда.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    /// TTL для tool'а, не упомянутого явно. Если 0 — tool считается
    /// non-cacheable.
    #[serde(default = "default_default_ttl")]
    pub default_ttl_seconds: u64,
    /// Поименованные правила. Ключ — имя tool'а как приходит в `tools/call`.
    #[serde(default)]
    pub tools: HashMap<String, ToolPolicy>,
    /// Per-scope override. Ключ — значение `repo` для cache-ci.
    /// Если scope не упомянут — считается cacheable (default).
    /// Целевой use case — отключение кэша на federated репо при групповой
    /// работе: `[scopes.ut] cacheable = false` → все ответы по `repo=ut` идут
    /// напрямую через federation forward, без сохранения в кэш.
    #[serde(default)]
    pub scopes: HashMap<String, ScopePolicy>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            default_ttl_seconds: default_default_ttl(),
            tools: HashMap::new(),
            scopes: HashMap::new(),
        }
    }
}

fn default_default_ttl() -> u64 {
    600
}

/// Правило для одного tool'а.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Если `false` — tool никогда не кэшируется (например, `execute_query`,
    /// `eventlog_query`).
    #[serde(default = "default_cacheable")]
    pub cacheable: bool,
    /// TTL в секундах, если `cacheable = true`. None — берётся `default_ttl_seconds`
    /// из корня политики.
    pub ttl_seconds: Option<u64>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            cacheable: true,
            ttl_seconds: None,
        }
    }
}

/// Per-scope правило кэширования. Сейчас единственный параметр — `cacheable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopePolicy {
    /// Если `false` — все запросы с этим scope (значение `repo`/`base`) идут
    /// напрямую через бэкенд, без сохранения в кэш. Default — `true`.
    #[serde(default = "default_cacheable")]
    pub cacheable: bool,
}

impl Default for ScopePolicy {
    fn default() -> Self {
        Self { cacheable: true }
    }
}

fn default_cacheable() -> bool {
    true
}

impl Policy {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать политику: {}", path.display()))?;
        let p: Policy = toml::from_str(&raw)
            .with_context(|| format!("не удалось разобрать TOML: {}", path.display()))?;
        Ok(p)
    }

    /// Решить, кэшировать ли scope (значение `repo`/`base`). Default — `true`.
    /// Если scope явно перечислен в `[scopes.<alias>]` с `cacheable = false`,
    /// возвращает `false` — прокси должен форвардить такие запросы без
    /// сохранения в кэш.
    pub fn is_scope_cacheable(&self, scope: &str) -> bool {
        self.scopes
            .get(scope)
            .map(|rule| rule.cacheable)
            .unwrap_or(true)
    }

    /// Решить, кэшировать ли tool, и если да — на сколько секунд.
    /// Возвращает `None` если tool помечен `cacheable = false`.
    pub fn ttl_for(&self, tool: &str) -> Option<u64> {
        match self.tools.get(tool) {
            Some(rule) if !rule.cacheable => None,
            Some(rule) => Some(rule.ttl_seconds.unwrap_or(self.default_ttl_seconds)),
            None => {
                if self.default_ttl_seconds == 0 {
                    None
                } else {
                    Some(self.default_ttl_seconds)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_specific_ttl_wins_over_default() {
        let toml_str = r#"
            default_ttl_seconds = 600

            [tools."search_function"]
            ttl_seconds = 3600

            [tools."execute_query"]
            cacheable = false
        "#;
        let p: Policy = toml::from_str(toml_str).unwrap();
        assert_eq!(p.ttl_for("search_function"), Some(3600));
        assert_eq!(p.ttl_for("execute_query"), None);
        assert_eq!(p.ttl_for("unknown_tool"), Some(600));
    }

    #[test]
    fn default_zero_means_global_disable() {
        let toml_str = r#"
            default_ttl_seconds = 0

            [tools."get_metadata_structure"]
            ttl_seconds = 3600
        "#;
        let p: Policy = toml::from_str(toml_str).unwrap();
        assert_eq!(p.ttl_for("unknown"), None);
        assert_eq!(p.ttl_for("get_metadata_structure"), Some(3600));
    }

    #[test]
    fn is_scope_cacheable_default_true_when_no_override() {
        let p = Policy::default();
        assert!(p.is_scope_cacheable("ut"));
        assert!(p.is_scope_cacheable("bp-ss"));
        assert!(p.is_scope_cacheable("smaks-ut"));
        assert!(p.is_scope_cacheable(""));
    }

    #[test]
    fn is_scope_cacheable_respects_explicit_false() {
        let toml_str = r#"
            default_ttl_seconds = 600

            [scopes.ut]
            cacheable = false

            [scopes."bp-ss"]
            cacheable = true
        "#;
        let p: Policy = toml::from_str(toml_str).unwrap();
        assert!(!p.is_scope_cacheable("ut"), "ut explicit disabled");
        assert!(p.is_scope_cacheable("bp-ss"), "bp-ss explicit enabled");
        assert!(p.is_scope_cacheable("tdk-bp"), "tdk-bp default = true");
    }

    #[test]
    fn scope_override_independent_of_tool_ttl() {
        let toml_str = r#"
            default_ttl_seconds = 600

            [tools."search_function"]
            ttl_seconds = 3600

            [scopes.ut]
            cacheable = false
        "#;
        let p: Policy = toml::from_str(toml_str).unwrap();
        // ttl_for всё равно даёт 3600 — он не зависит от scope. Решение «писать
        // в кэш или нет» принимается на уровне proxy.rs по комбинации
        // is_scope_cacheable + ttl_for.
        assert_eq!(p.ttl_for("search_function"), Some(3600));
        assert!(!p.is_scope_cacheable("ut"));
        assert!(p.is_scope_cacheable("bp-ss"));
    }
}
