//! Состояние «заморозки» прокси (block-режим).
//!
//! Сценарий: внешний триггер (sidecar, ручной вызов) обнаружил, что бэкенд
//! сейчас отдаёт устаревшие данные (например, началась крупная переиндексация
//! всего репо, и в полусобранном состоянии данные неконсистентны). Чтобы
//! клиент не получал старые ответы:
//!
//! - Прокси переходит в режим `frozen` для конкретного scope (или global) на TTL.
//! - На любой `tools/call` в замороженный scope возвращается ошибка
//!   «backend updating, retry after N seconds» — клиент явно понимает, что данных
//!   нет, и должен повторить позже. Старые кэшированные ответы НЕ отдаются.
//!
//! Snapshot-семантика через `arc_swap::ArcSwap` — чтения дешёвые (на каждый
//! `tools/call`), записи редкие (триггер с интервалом минуты).
//!
//! Scope = строка, обычно соответствует значению параметра `repo`. Пустая
//! строка `""` зарезервирована под global-freeze (срабатывает на любой scope).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use serde::Serialize;

/// Снимок текущего состояния заморозок: scope → момент истечения.
///
/// Хранится в `ArcSwap`, поэтому копируется и заменяется целиком при каждом
/// freeze/thaw (это допустимо — записей единицы).
#[derive(Debug, Default, Clone)]
pub struct FreezeState {
    /// Ключ — scope (`""` = global, `"<repo>"` или `"<base>"` для селективного).
    pub frozen: HashMap<String, Instant>,
}

impl FreezeState {
    /// Если scope сейчас заморожен — вернуть оставшийся TTL. Иначе `None`.
    /// Учитывается и точное совпадение scope, и global-заморозка (`""`).
    pub fn is_frozen(&self, scope: &str) -> Option<Duration> {
        let now = Instant::now();
        // Сначала global — если стоит, перекрывает всё.
        if let Some(until) = self.frozen.get("") {
            if *until > now {
                return Some(*until - now);
            }
        }
        // Потом конкретный scope.
        if !scope.is_empty() {
            if let Some(until) = self.frozen.get(scope) {
                if *until > now {
                    return Some(*until - now);
                }
            }
        }
        None
    }
}

/// Контроллер заморозок. `Clone` — это копия `Arc`, состояние общее.
#[derive(Clone, Default)]
pub struct FreezeController {
    state: Arc<ArcSwap<FreezeState>>,
}

impl FreezeController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Заморозить scope на указанный TTL. Если scope пустой — global-freeze.
    /// Если для scope уже стоит более поздний `until` — он сохраняется (берём max).
    pub fn freeze(&self, scope: &str, duration: Duration) {
        let new_until = Instant::now() + duration;
        let snap = self.state.load();
        let mut next = (**snap).clone();
        next.frozen
            .entry(scope.to_string())
            .and_modify(|cur| {
                if new_until > *cur {
                    *cur = new_until;
                }
            })
            .or_insert(new_until);
        self.state.store(Arc::new(next));
    }

    /// Снять заморозку с конкретного scope. Если такого нет — no-op.
    pub fn thaw(&self, scope: &str) {
        let snap = self.state.load();
        if !snap.frozen.contains_key(scope) {
            return;
        }
        let mut next = (**snap).clone();
        next.frozen.remove(scope);
        self.state.store(Arc::new(next));
    }

    /// Снять все заморозки.
    pub fn thaw_all(&self) {
        self.state.store(Arc::new(FreezeState::default()));
    }

    /// Проверить, заморожен ли scope.
    pub fn is_frozen(&self, scope: &str) -> Option<Duration> {
        self.state.load().is_frozen(scope)
    }

    /// Список активных (не истёкших) заморозок: (scope, ttl_remaining_seconds).
    /// Используется в `/status` endpoint.
    pub fn list_active(&self) -> Vec<FreezeEntry> {
        let now = Instant::now();
        let snap = self.state.load();
        snap.frozen
            .iter()
            .filter_map(|(scope, until)| {
                if *until > now {
                    Some(FreezeEntry {
                        scope: scope.clone(),
                        ttl_remaining_seconds: (*until - now).as_secs(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Запись о заморозке для сериализации в `/status` ответ.
#[derive(Debug, Clone, Serialize)]
pub struct FreezeEntry {
    pub scope: String,
    pub ttl_remaining_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn freeze_then_is_frozen_returns_some() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_secs(60));
        assert!(ctrl.is_frozen("ut").is_some());
        assert!(ctrl.is_frozen("bp-ss").is_none());
    }

    #[test]
    fn global_freeze_covers_any_scope() {
        let ctrl = FreezeController::new();
        ctrl.freeze("", Duration::from_secs(60));
        assert!(ctrl.is_frozen("ut").is_some());
        assert!(ctrl.is_frozen("anything").is_some());
        assert!(ctrl.is_frozen("").is_some());
    }

    #[test]
    fn thaw_removes_only_specified_scope() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_secs(60));
        ctrl.freeze("bp-ss", Duration::from_secs(60));
        ctrl.thaw("ut");
        assert!(ctrl.is_frozen("ut").is_none());
        assert!(ctrl.is_frozen("bp-ss").is_some());
    }

    #[test]
    fn thaw_all_clears_everything() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_secs(60));
        ctrl.freeze("", Duration::from_secs(60));
        ctrl.thaw_all();
        assert!(ctrl.is_frozen("ut").is_none());
        assert!(ctrl.is_frozen("any").is_none());
    }

    #[test]
    fn expired_freeze_is_ignored() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_millis(20));
        sleep(Duration::from_millis(50));
        assert!(ctrl.is_frozen("ut").is_none());
        let active = ctrl.list_active();
        assert!(active.is_empty(), "истёкшая заморозка не должна попадать в list_active");
    }

    #[test]
    fn freeze_takes_max_ttl_on_overlap() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_secs(10));
        ctrl.freeze("ut", Duration::from_secs(60));
        let ttl = ctrl.is_frozen("ut").unwrap();
        assert!(ttl.as_secs() > 30, "более длинная заморозка должна победить, получили {ttl:?}");
    }

    #[test]
    fn shorter_freeze_does_not_shrink_existing() {
        let ctrl = FreezeController::new();
        ctrl.freeze("ut", Duration::from_secs(60));
        ctrl.freeze("ut", Duration::from_secs(5));
        let ttl = ctrl.is_frozen("ut").unwrap();
        assert!(ttl.as_secs() > 30, "короткая заморозка не должна укорачивать длинную, получили {ttl:?}");
    }
}
