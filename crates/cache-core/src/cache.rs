//! In-memory кэш на DashMap с TTL-эвикцией.
//!
//! Ключ — структурный: `<server_alias>|<scope>|<tool>|<sha256_hex(args)>`.
//! Префикс из plain-text полей нужен для селективной инвалидации:
//! `invalidate_where(|k| k.starts_with("ci|ut|"))` сносит только записи
//! `repo=ut`, не трогая `bp-ss`/`bp-tdk`/`zup`. Хвост sha256 даёт уникальность
//! и нечувствителен к порядку полей в args (через `normalize_args`).
//!
//! Scope = значение `args.repo`. Если в args такого ключа нет — scope пустой (`""`).
//! Scope **не должен содержать `|`** — иначе разделитель префикса станет
//! неоднозначным. В наших алиасах (`ut`, `bp-ss`, `smaks-ut`) пайпов нет.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::reverse_index::ReverseIndex;

/// Запись кэша.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Сериализованный JSON-ответ от бэкенда. Храним именно сырой JSON-текст,
    /// а не Value: при отдаче клиенту копирование байтов дешевле, чем
    /// повторная сериализация Value.
    pub payload: Arc<String>,
    /// Когда запись истечёт.
    pub expires_at: Instant,
    /// Когда записали (для метрик и отладки).
    pub recorded_at: Instant,
    /// Список файлов, на которых построен этот ответ (из `_meta.dependent_files`
    /// в JSON-ответе бэкенда). Пустой для ответов без metadata.
    /// Хранится здесь же, чтобы при `evict_expired`/`invalidate_where` можно
    /// было корректно почистить и `ReverseIndex` (иначе там останутся висящие
    /// `cache_key` без записи в основном кэше).
    pub dependent_files: Vec<String>,
}

impl CacheEntry {
    /// Запись без зависимостей от файлов (бэкенд не прислал `_meta.dependent_files`).
    pub fn new(payload: Arc<String>, ttl: Duration) -> Self {
        Self::new_with_deps(payload, ttl, Vec::new())
    }

    /// Запись с явным списком зависимостей.
    pub fn new_with_deps(
        payload: Arc<String>,
        ttl: Duration,
        dependent_files: Vec<String>,
    ) -> Self {
        let now = Instant::now();
        Self {
            payload,
            expires_at: now + ttl,
            recorded_at: now,
            dependent_files,
        }
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Сам кэш. Ключ — String (hex-hash), значение — CacheEntry.
/// DashMap внутри уже потокобезопасный, дополнительных блокировок не нужно.
///
/// Дополнительно кэш ведёт `reverse_index` для точечной инвалидации по
/// `file_path`: когда бэкенд при ответе на read-tool прислал список
/// `_meta.dependent_files`, эти связи запоминаются, и затем по сигналу
/// `POST /invalidate {file_paths: [...]}` от daemon (после переиндексации
/// файла) можно мгновенно снести только зависимые записи, не задевая
/// соседних.
#[derive(Debug, Default)]
pub struct Cache {
    inner: DashMap<String, CacheEntry>,
    reverse_index: ReverseIndex,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Стабильный ключ кэша. Структурный prefix позволяет селективно
    /// инвалидировать через `invalidate_where(|k| k.starts_with(prefix))`.
    /// Хвост `sha256(args)` гарантирует уникальность и не зависит от порядка
    /// полей в args (через `normalize_args`).
    ///
    /// Формат: `{server_alias}|{scope}|{tool}|{sha256_hex}`
    /// Пример: `ci|ut|search_function|9f86d081…`
    /// Если scope не задан — между двумя `|` пусто: `ci||get_stats|9f86…`.
    pub fn key_for(server_alias: &str, scope: &str, tool: &str, args: &Value) -> String {
        let normalized = normalize_args(args);
        let mut hasher = Sha256::new();
        hasher.update(normalized.as_bytes());
        let hash_hex = hex::encode(hasher.finalize());
        format!("{server_alias}|{scope}|{tool}|{hash_hex}")
    }

    /// Префикс ключа для селективной инвалидации по `repo`/`base`.
    /// `Cache::scope_prefix("ci", "ut")` → `"ci|ut|"`.
    pub fn scope_prefix(server_alias: &str, scope: &str) -> String {
        format!("{server_alias}|{scope}|")
    }

    /// Попытаться получить живую запись. Если истёкшая — удалить и вернуть None.
    pub fn get(&self, key: &str) -> Option<Arc<String>> {
        // Сначала read-only попытка.
        if let Some(entry) = self.inner.get(key) {
            if !entry.is_expired() {
                return Some(entry.payload.clone());
            }
        }
        // Истекла — удаляем (entry-API чтобы не словить race).
        self.inner.remove_if(key, |_, v| v.is_expired());
        None
    }

    /// Как [`get`], но дополнительно возвращает `dependent_files` живой записи —
    /// нужно для проверки dirty-флагов в proxy (write-triggered ленивая
    /// ревалидация, #1471): по списку зависимых файлов решаем, не «грязный» ли
    /// какой-то из них, прежде чем отдать запись как HIT.
    ///
    /// [`get`]: Cache::get
    pub fn get_with_deps(&self, key: &str) -> Option<(Arc<String>, Vec<String>)> {
        if let Some(entry) = self.inner.get(key) {
            if !entry.is_expired() {
                return Some((entry.payload.clone(), entry.dependent_files.clone()));
            }
        }
        self.inner.remove_if(key, |_, v| v.is_expired());
        None
    }

    pub fn insert(&self, key: String, payload: Arc<String>, ttl: Duration) {
        self.inner.insert(key, CacheEntry::new(payload, ttl));
    }

    /// Записать ответ в кэш с явным списком файлов, на которых он построен.
    /// Одновременно регистрирует все `file_path → key` связи в `reverse_index`.
    /// Если `dependent_files` пуст — поведение эквивалентно `insert`.
    pub fn insert_with_deps(
        &self,
        key: String,
        payload: Arc<String>,
        ttl: Duration,
        dependent_files: Vec<String>,
    ) {
        // Сначала наполняем reverse_index, потом сам кэш — порядок не критичен,
        // но так логичнее: на момент когда entry виден в main cache, его связи
        // в индексе уже есть.
        for path in &dependent_files {
            self.reverse_index.associate(path, &key);
        }
        self.inner.insert(
            key,
            CacheEntry::new_with_deps(payload, ttl, dependent_files),
        );
    }

    /// Точечная инвалидация по списку файлов: сносит все cache_keys, зависящие
    /// хотя бы от одного из указанных `file_paths`, и **полностью** очищает
    /// `reverse_index` от ссылок на снесённые keys. Возвращает число снесённых
    /// entries из main cache.
    ///
    /// Чистка по всем `dependent_files` каждой entry — каждый снесённый key
    /// может зависеть от файлов **за пределами** переданных `file_paths`.
    /// Если просто удалить только `file_paths`, то в `reverse_index` останутся
    /// стейл-ссылки на уже несуществующие cache_keys (они не вредят
    /// корректности, но раздувают индекс).
    ///
    /// Соседние entries (построенные на других файлах) остаются живыми.
    pub fn invalidate_files(&self, file_paths: &[String]) -> usize {
        let keys = self.reverse_index.keys_for_files(file_paths);
        let mut removed = 0;
        // Сначала собираем все dependent_files снесённых entries, потом одним
        // проходом чистим reverse_index. Делать чистку внутри remove-цикла
        // нельзя — DashMap может deadlock'нуть при попытке взять get_mut
        // на entry, которая удаляется в параллельном потоке.
        let mut to_clean: Vec<(String, Vec<String>)> = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some((_, entry)) = self.inner.remove(key) {
                removed += 1;
                to_clean.push((key.clone(), entry.dependent_files));
            }
        }
        for (key, deps) in to_clean {
            // remove_key_from_files сам удалит file_path из reverse_index,
            // если для него больше не осталось cache_keys.
            self.reverse_index.remove_key_from_files(&deps, &key);
        }
        removed
    }

    /// Удалить все записи, ключ которых совпадает с предикатом.
    /// Используется в `/invalidate`. Дополнительно чистит `reverse_index`
    /// от сносимых ключей — иначе там останутся висящие ссылки.
    pub fn invalidate_where<F>(&self, mut predicate: F) -> usize
    where
        F: FnMut(&str) -> bool,
    {
        let mut removed = 0;
        let mut to_clean: Vec<(String, Vec<String>)> = Vec::new();
        self.inner.retain(|k, v| {
            let keep = !predicate(k);
            if !keep {
                removed += 1;
                to_clean.push((k.clone(), v.dependent_files.clone()));
            }
            keep
        });
        for (key, paths) in to_clean {
            self.reverse_index.remove_key_from_files(&paths, &key);
        }
        removed
    }

    /// Принудительная инвалидация всего кэша. Reverse_index тоже очищается.
    pub fn clear(&self) {
        self.inner.clear();
        self.reverse_index.clear();
    }

    /// Удалить все истёкшие записи (фоновая эвикция). Дополнительно чистит
    /// `reverse_index` от сносимых ключей.
    /// Возвращает количество удалённых записей.
    pub fn evict_expired(&self) -> usize {
        let mut removed = 0;
        let mut to_clean: Vec<(String, Vec<String>)> = Vec::new();
        self.inner.retain(|k, v| {
            let alive = !v.is_expired();
            if !alive {
                removed += 1;
                to_clean.push((k.clone(), v.dependent_files.clone()));
            }
            alive
        });
        for (key, paths) in to_clean {
            self.reverse_index.remove_key_from_files(&paths, &key);
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Размер реверс-индекса (число файлов с зависимостями).
    pub fn reverse_index_size(&self) -> usize {
        self.reverse_index.len()
    }
}

/// Стабильная JSON-сериализация: ключи объектов сортируются.
/// `serde_json` сам по себе порядок ключей сохраняет в порядке вставки,
/// поэтому делаем рекурсивный обход и пересобираем.
fn normalize_args(value: &Value) -> String {
    let normalized = sort_keys(value.clone());
    serde_json::to_string(&normalized).unwrap_or_else(|_| String::from("null"))
}

fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                sorted.insert(k, v);
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread::sleep;

    #[test]
    fn key_is_stable_for_equivalent_args() {
        let a = json!({"repo": "ut", "query": "search_function"});
        let b = json!({"query": "search_function", "repo": "ut"});
        assert_eq!(
            Cache::key_for("ci", "ut", "x", &a),
            Cache::key_for("ci", "ut", "x", &b),
        );
    }

    #[test]
    fn key_starts_with_structural_prefix() {
        let args = json!({"repo": "ut", "query": "X"});
        let key = Cache::key_for("ci", "ut", "search_function", &args);
        assert!(
            key.starts_with("ci|ut|search_function|"),
            "ожидался structural prefix, получили {key}"
        );
    }

    #[test]
    fn scope_prefix_format() {
        assert_eq!(Cache::scope_prefix("ci", "ut"), "ci|ut|");
        assert_eq!(Cache::scope_prefix("1c", "smaks-ut"), "1c|smaks-ut|");
    }

    #[test]
    fn keys_with_different_scope_differ() {
        let args = json!({"query": "X"});
        let k_ut = Cache::key_for("ci", "ut", "search_function", &args);
        let k_bp = Cache::key_for("ci", "bp-ss", "search_function", &args);
        assert_ne!(k_ut, k_bp);
        assert!(k_ut.starts_with("ci|ut|"));
        assert!(k_bp.starts_with("ci|bp-ss|"));
    }

    #[test]
    fn get_returns_none_after_ttl() {
        let cache = Cache::new();
        let key = "k1".to_string();
        cache.insert(key.clone(), Arc::new("payload".into()), Duration::from_millis(50));
        assert!(cache.get(&key).is_some());
        sleep(Duration::from_millis(80));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn evict_expired_drops_only_expired() {
        let cache = Cache::new();
        cache.insert("alive".into(), Arc::new("p".into()), Duration::from_secs(60));
        cache.insert("dead".into(), Arc::new("p".into()), Duration::from_millis(10));
        sleep(Duration::from_millis(40));
        let removed = cache.evict_expired();
        assert_eq!(removed, 1);
        assert!(cache.get("alive").is_some());
        assert!(cache.get("dead").is_none());
    }

    #[test]
    fn invalidate_where_drops_matched() {
        let cache = Cache::new();
        cache.insert("a1".into(), Arc::new("p".into()), Duration::from_secs(60));
        cache.insert("b2".into(), Arc::new("p".into()), Duration::from_secs(60));
        cache.insert("a3".into(), Arc::new("p".into()), Duration::from_secs(60));
        let removed = cache.invalidate_where(|k| k.starts_with('a'));
        assert_eq!(removed, 2);
        assert!(cache.get("b2").is_some());
        assert!(cache.get("a1").is_none());
    }

    #[test]
    fn insert_with_deps_registers_reverse_index() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "key1".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into(), "src/Y.bsl".into()],
        );
        assert_eq!(cache.reverse_index_size(), 2);
        assert!(cache.get("key1").is_some());
    }

    #[test]
    fn invalidate_files_drops_only_dependent_entries() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "key_X".into(),
            Arc::new("payload_x".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into()],
        );
        cache.insert_with_deps(
            "key_Y".into(),
            Arc::new("payload_y".into()),
            Duration::from_secs(60),
            vec!["src/Y.bsl".into()],
        );
        cache.insert_with_deps(
            "key_XY".into(),
            Arc::new("payload_xy".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into(), "src/Y.bsl".into()],
        );

        // Сносим только записи, зависящие от X.bsl.
        let removed = cache.invalidate_files(&["src/X.bsl".into()]);
        assert_eq!(removed, 2, "сносим key_X и key_XY");
        assert!(cache.get("key_X").is_none());
        assert!(cache.get("key_XY").is_none());
        assert!(cache.get("key_Y").is_some(), "key_Y не зависит от X.bsl");

        // src/X.bsl ушёл из индекса, src/Y.bsl остался (от него ещё зависит key_Y).
        assert_eq!(cache.reverse_index_size(), 1);
    }

    #[test]
    fn invalidate_files_empty_paths_noop() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "key1".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into()],
        );
        let removed = cache.invalidate_files(&[]);
        assert_eq!(removed, 0);
        assert!(cache.get("key1").is_some());
    }

    #[test]
    fn invalidate_files_unknown_path_noop() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "key1".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into()],
        );
        let removed = cache.invalidate_files(&["src/UNKNOWN.bsl".into()]);
        assert_eq!(removed, 0);
        assert!(cache.get("key1").is_some());
        assert_eq!(cache.reverse_index_size(), 1);
    }

    #[test]
    fn invalidate_files_fully_cleans_reverse_index_from_dangling_refs() {
        // Сценарий из боевого теста: entry с многими dependent_files. Touch
        // ОДНОГО файла должен снести entry И ПОЛНОСТЬЮ убрать его из
        // reverse_index по всем другим путям — иначе там остаются стейл-ссылки.
        let cache = Cache::new();
        cache.insert_with_deps(
            "key_multi".into(),
            Arc::new("payload".into()),
            Duration::from_secs(60),
            vec![
                "src/A.bsl".into(),
                "src/B.bsl".into(),
                "src/C.bsl".into(),
                "src/D.bsl".into(),
            ],
        );
        // Соседняя entry, которая зависит от B и от своего E. После инвалидации
        // по A → B и E должны остаться (есть key_other), все остальные удалены.
        cache.insert_with_deps(
            "key_other".into(),
            Arc::new("payload2".into()),
            Duration::from_secs(60),
            vec!["src/B.bsl".into(), "src/E.bsl".into()],
        );
        assert_eq!(cache.reverse_index_size(), 5); // A, B, C, D, E

        // Touch только A — но в key_multi есть зависимости от B, C, D тоже.
        let removed = cache.invalidate_files(&["src/A.bsl".into()]);
        assert_eq!(removed, 1, "снесён только key_multi");
        assert!(cache.get("key_other").is_some(), "key_other жив");
        assert_eq!(cache.len(), 1);

        // reverse_index должен содержать ровно B и E (от живой key_other).
        // A, C, D — удалены (key_multi не существует, других keys нет).
        assert_eq!(
            cache.reverse_index_size(),
            2,
            "ровно 2 path остались: B (от key_other) и E (от key_other)"
        );

        // Двойная проверка: invalidate по C/D больше ничего не сносит, потому
        // что они УЖЕ удалены из reverse_index (а не висят как стейл-ссылки).
        let removed_again = cache.invalidate_files(&["src/C.bsl".into(), "src/D.bsl".into()]);
        assert_eq!(removed_again, 0, "стейл-ссылок нет — ничего сносить");
        assert_eq!(cache.reverse_index_size(), 2);
    }

    #[test]
    fn evict_expired_cleans_reverse_index() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "dead".into(),
            Arc::new("p".into()),
            Duration::from_millis(10),
            vec!["src/X.bsl".into()],
        );
        cache.insert_with_deps(
            "alive".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/Y.bsl".into()],
        );
        assert_eq!(cache.reverse_index_size(), 2);

        sleep(Duration::from_millis(40));
        let removed = cache.evict_expired();
        assert_eq!(removed, 1);
        // src/X.bsl остался без зависимостей → выкинут из индекса.
        assert_eq!(cache.reverse_index_size(), 1);
    }

    #[test]
    fn invalidate_where_cleans_reverse_index() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "ci|ut|grep_body|h1".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into()],
        );
        cache.insert_with_deps(
            "ci|bp-ss|grep_body|h2".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/Y.bsl".into()],
        );
        let removed = cache.invalidate_where(|k| k.starts_with("ci|ut|"));
        assert_eq!(removed, 1);
        // src/X.bsl остался без зависимостей → удалён из индекса.
        assert_eq!(cache.reverse_index_size(), 1);
        assert!(cache.get("ci|bp-ss|grep_body|h2").is_some());
    }

    #[test]
    fn clear_drops_reverse_index_too() {
        let cache = Cache::new();
        cache.insert_with_deps(
            "key1".into(),
            Arc::new("p".into()),
            Duration::from_secs(60),
            vec!["src/X.bsl".into()],
        );
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.reverse_index_size(), 0);
    }
}
