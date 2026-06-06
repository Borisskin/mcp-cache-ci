//! Множество «грязных» путей для write-triggered ленивой ревалидации (#1471).
//!
//! Демон `code-index` при FS-событии шлёт `POST /mark-dirty {repo, files:[{path,
//! mtime}]}` ДО переразбора/commit. Прокси кладёт `(scope, rel_path) → observed
//! mtime` сюда. Дальше на каждом чтении, чья запись зависит от грязного файла,
//! прокси форвардит на backend и сверяет observed-mtime с индексным mtime из
//! ответа serve (`_meta.file_mtimes`): кэширует ответ только когда индекс догнал
//! диск (`index_mtime >= observed`), тогда же снимает флаг.
//!
//! Ключ — `(scope, rel_path)`, а не просто `rel_path`: один и тот же
//! относительный путь может существовать в разных репо, а сверка mtime должна
//! идти строго в пределах своего scope (значение `repo` в tool-call).
//!
//! Без FS-доступа: «текущий» mtime приносит демон (он co-located с файлами),
//! поэтому механизм работает и для федеративных репо, чьи файлы прокси не видит.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Запись о грязном файле.
#[derive(Debug, Clone)]
struct DirtyEntry {
    /// Максимальный наблюдённый демоном mtime (unix-секунды). При нескольких
    /// записях в файл держим максимум — индекс должен догнать самую свежую.
    observed: i64,
    /// Когда пометили (для TTL-страховки `prune_older_than`).
    marked_at: Instant,
}

/// Потокобезопасное множество грязных путей. Ключ — `(scope, rel_path)`.
#[derive(Debug, Default)]
pub struct DirtySet {
    inner: DashMap<(String, String), DirtyEntry>,
}

impl DirtySet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Пометить путь грязным с observed-mtime. Держим максимум observed,
    /// `marked_at` обновляем на каждый сигнал.
    pub fn mark(&self, scope: &str, path: &str, observed_mtime: i64) {
        let key = (scope.to_string(), path.to_string());
        self.inner
            .entry(key)
            .and_modify(|e| {
                if observed_mtime > e.observed {
                    e.observed = observed_mtime;
                }
                e.marked_at = Instant::now();
            })
            .or_insert(DirtyEntry {
                observed: observed_mtime,
                marked_at: Instant::now(),
            });
    }

    /// observed-mtime пути в этом scope, если он грязный.
    pub fn observed(&self, scope: &str, path: &str) -> Option<i64> {
        self.inner
            .get(&(scope.to_string(), path.to_string()))
            .map(|e| e.observed)
    }

    /// Есть ли среди `paths` хоть один грязный в этом scope. Быстрый путь:
    /// пустое множество → сразу false (без аллокаций ключей).
    pub fn any_dirty(&self, scope: &str, paths: &[String]) -> bool {
        if self.inner.is_empty() {
            return false;
        }
        paths
            .iter()
            .any(|p| self.inner.contains_key(&(scope.to_string(), p.clone())))
    }

    /// Снять флаг с пути, если индекс догнал диск (`index_mtime >= observed`).
    /// Если за время сверки observed вырос (пришёл новый mark-dirty) — не снимем
    /// (remove_if перечитает актуальный observed).
    pub fn clear_if_caught_up(&self, scope: &str, path: &str, index_mtime: i64) {
        let key = (scope.to_string(), path.to_string());
        self.inner.remove_if(&key, |_, e| index_mtime >= e.observed);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Удалить флаги старше `ttl` — страховка от утечки, если по грязному пути
    /// так и не пришло ни одного чтения (флаг снимается только сверкой на
    /// чтении). Возвращает число удалённых.
    pub fn prune_older_than(&self, ttl: Duration) -> usize {
        let mut removed = 0;
        self.inner.retain(|_, e| {
            let keep = e.marked_at.elapsed() < ttl;
            if !keep {
                removed += 1;
            }
            keep
        });
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_keeps_max_observed() {
        let d = DirtySet::new();
        d.mark("ut", "src/X.bsl", 100);
        d.mark("ut", "src/X.bsl", 90); // меньше — не перетирает
        assert_eq!(d.observed("ut", "src/X.bsl"), Some(100));
        d.mark("ut", "src/X.bsl", 150); // больше — обновляет
        assert_eq!(d.observed("ut", "src/X.bsl"), Some(150));
    }

    #[test]
    fn scope_isolation() {
        let d = DirtySet::new();
        d.mark("ut", "src/X.bsl", 100);
        assert!(d.any_dirty("ut", &["src/X.bsl".into()]));
        // Тот же путь в другом scope — не грязный.
        assert!(!d.any_dirty("bp-ss", &["src/X.bsl".into()]));
        assert_eq!(d.observed("bp-ss", "src/X.bsl"), None);
    }

    #[test]
    fn any_dirty_empty_is_false_fast() {
        let d = DirtySet::new();
        assert!(!d.any_dirty("ut", &["a".into(), "b".into()]));
    }

    #[test]
    fn clear_if_caught_up_respects_observed() {
        let d = DirtySet::new();
        d.mark("ut", "src/X.bsl", 100);
        // Индекс отстаёт — флаг остаётся.
        d.clear_if_caught_up("ut", "src/X.bsl", 99);
        assert_eq!(d.observed("ut", "src/X.bsl"), Some(100));
        // Индекс догнал — флаг снят.
        d.clear_if_caught_up("ut", "src/X.bsl", 100);
        assert_eq!(d.observed("ut", "src/X.bsl"), None);
    }

    #[test]
    fn prune_removes_old_entries() {
        let d = DirtySet::new();
        d.mark("ut", "src/X.bsl", 100);
        // ttl=0 → всё старше нуля удаляется.
        let removed = d.prune_older_than(Duration::from_millis(0));
        assert_eq!(removed, 1);
        assert!(d.is_empty());
    }
}
