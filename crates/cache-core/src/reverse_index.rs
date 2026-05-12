//! Реверсивный индекс `file_path → cache_keys`.
//!
//! Назначение: при наполнении кэша cache-ci узнаёт от бэкенда (через
//! `_meta.dependent_files` в JSON-ответе), на каких файлах построен ответ.
//! Эти связи запоминаются здесь. Когда приходит сигнал «файл X изменился»
//! (POST `/invalidate {file_paths: [X]}` от daemon после commit SQLite) —
//! cache-ci по реверс-индексу мгновенно находит все cache_keys, зависящие
//! от X, и сносит их точечно, не задевая ответов, построенных на других
//! файлах.
//!
//! Все операции — атомарные через DashMap entry API.

use std::collections::HashSet;

use dashmap::DashMap;

/// `file_path` → набор `cache_keys`, зависящих от этого файла.
#[derive(Debug, Default)]
pub struct ReverseIndex {
    inner: DashMap<String, HashSet<String>>,
}

impl ReverseIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Запомнить, что `cache_key` зависит от `file_path`.
    pub fn associate(&self, file_path: &str, cache_key: &str) {
        self.inner
            .entry(file_path.to_string())
            .or_default()
            .insert(cache_key.to_string());
    }

    /// Собрать все `cache_keys`, зависящие хотя бы от одного из указанных
    /// файлов. Дубликаты дедуплицированы.
    pub fn keys_for_files(&self, file_paths: &[String]) -> Vec<String> {
        let mut keys: HashSet<String> = HashSet::new();
        for path in file_paths {
            if let Some(set) = self.inner.get(path) {
                keys.extend(set.iter().cloned());
            }
        }
        keys.into_iter().collect()
    }

    /// Полностью удалить связи для указанных файлов. Используется при
    /// `/invalidate {file_paths}` — после сноса cache_entries отдельные file_path
    /// больше никому не нужны.
    pub fn remove_files(&self, file_paths: &[String]) {
        for path in file_paths {
            self.inner.remove(path);
        }
    }

    /// Удалить `cache_key` из всех связей с указанными файлами. Используется
    /// при `evict_expired` и `invalidate_where`, когда из main cache уходит
    /// конкретный entry — нельзя оставлять висящий ключ в реверс-индексе.
    /// Если для file_path не осталось ни одного cache_key — file_path удаляется
    /// целиком (чтобы индекс не разрастался пустыми HashSet'ами).
    pub fn remove_key_from_files(&self, file_paths: &[String], cache_key: &str) {
        for path in file_paths {
            let became_empty = {
                if let Some(mut entry) = self.inner.get_mut(path) {
                    entry.remove(cache_key);
                    entry.is_empty()
                } else {
                    false
                }
            };
            if became_empty {
                self.inner.remove(path);
            }
        }
    }

    /// Полная очистка (для `Cache::clear`).
    pub fn clear(&self) {
        self.inner.clear();
    }

    /// Число файлов в индексе (для метрики `reverse_index_size`).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associate_and_lookup() {
        let ri = ReverseIndex::new();
        ri.associate("src/X.bsl", "ci|ut|grep_body|abc");
        ri.associate("src/Y.bsl", "ci|ut|grep_body|abc");
        ri.associate("src/X.bsl", "ci|ut|search_function|def");

        let mut keys = ri.keys_for_files(&["src/X.bsl".into()]);
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "ci|ut|grep_body|abc".to_string(),
                "ci|ut|search_function|def".to_string()
            ]
        );

        let keys_y = ri.keys_for_files(&["src/Y.bsl".into()]);
        assert_eq!(keys_y, vec!["ci|ut|grep_body|abc"]);
    }

    #[test]
    fn keys_for_multiple_files_are_deduplicated() {
        let ri = ReverseIndex::new();
        ri.associate("src/X.bsl", "key1");
        ri.associate("src/Y.bsl", "key1"); // тот же ключ
        ri.associate("src/Y.bsl", "key2");

        let mut keys = ri.keys_for_files(&["src/X.bsl".into(), "src/Y.bsl".into()]);
        keys.sort();
        assert_eq!(keys, vec!["key1".to_string(), "key2".to_string()]);
    }

    #[test]
    fn remove_files_drops_entries() {
        let ri = ReverseIndex::new();
        ri.associate("src/X.bsl", "key1");
        ri.associate("src/Y.bsl", "key2");
        assert_eq!(ri.len(), 2);

        ri.remove_files(&["src/X.bsl".into()]);
        assert_eq!(ri.len(), 1);
        assert!(ri.keys_for_files(&["src/X.bsl".into()]).is_empty());
        assert_eq!(ri.keys_for_files(&["src/Y.bsl".into()]), vec!["key2"]);
    }

    #[test]
    fn remove_key_cleans_up_empty_sets() {
        let ri = ReverseIndex::new();
        ri.associate("src/X.bsl", "key1");
        ri.associate("src/X.bsl", "key2");
        assert_eq!(ri.len(), 1);

        ri.remove_key_from_files(&["src/X.bsl".into()], "key1");
        // ещё остался key2 — file_path не удаляется
        assert_eq!(ri.len(), 1);
        assert_eq!(ri.keys_for_files(&["src/X.bsl".into()]), vec!["key2"]);

        ri.remove_key_from_files(&["src/X.bsl".into()], "key2");
        // Теперь пусто — file_path удалён целиком
        assert_eq!(ri.len(), 0);
    }

    #[test]
    fn clear_drops_all() {
        let ri = ReverseIndex::new();
        ri.associate("src/X.bsl", "key1");
        ri.associate("src/Y.bsl", "key2");
        ri.clear();
        assert_eq!(ri.len(), 0);
    }
}
