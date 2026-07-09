//! HTTP-хэндлеры служебных эндпоинтов: /health, /metrics, /invalidate, /freeze, /thaw, /status.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use serde_json::json;

use cache_core::{Cache, DirtySet, FreezeController, Metrics};

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<Cache>,
    pub metrics: Arc<Metrics>,
    pub backend_url: String,
    /// `"ci"` — алиас сервера для построения scope-prefix кэш-ключа при
    /// селективной инвалидации.
    pub server_alias: String,
    /// Контроллер заморозки — общий с CacheProxy.
    pub freeze: FreezeController,
    /// Множество грязных путей — общее с CacheProxy (write-triggered ленивая
    /// ревалидация, #1471). Наполняется `POST /mark-dirty`.
    pub dirty: Arc<DirtySet>,
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.metrics.snapshot();
    Json(json!({
        "status": "ok",
        "backend_url": state.backend_url,
        "cache_size": snap.cache_size,
        "version": env!("CARGO_PKG_VERSION"),
        "server_alias": state.server_alias,
        "frozen_scopes": state.freeze.list_active(),
    }))
}

/// Prometheus text exposition format. Содержит counter'ы и gauges с label
/// `server="ci"`. Перед сериализацией синхронизирует `reverse_index_size` —
/// иначе значение могло устареть между insert/invalidate-операциями.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    state
        .metrics
        .update_reverse_index_size(state.cache.reverse_index_size());
    let text = state.metrics.to_prometheus_text(&state.server_alias);
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
}

/// JSON-снимок метрик. Совместимость со старыми клиентами (потребителями
/// `MetricsSnapshot`). Также удобен для curl + jq.
pub async fn metrics_json(State(state): State<AppState>) -> impl IntoResponse {
    state
        .metrics
        .update_reverse_index_size(state.cache.reverse_index_size());
    Json(state.metrics.snapshot())
}

#[derive(Debug, Deserialize)]
pub struct InvalidateRequest {
    /// Область кэша, записи которой снести. Родное имя параметра.
    #[serde(default)]
    pub scope: Option<String>,
    /// Устаревший синоним `scope` (историческое имя для code-index).
    #[serde(default)]
    pub repo: Option<String>,
    /// Устаревший синоним `scope` (историческое имя для 1c-router).
    #[serde(default)]
    pub base: Option<String>,
    /// Произвольный prefix ключа (например, для отладки). Имеет смысл, если
    /// нужен tool-уровень: `"ci|ut|search_function|"`.
    #[serde(default)]
    pub key_prefix: Option<String>,
    /// Снести весь кэш сервера.
    #[serde(default)]
    pub all: bool,
    /// Точечная инвалидация по списку файлов. Сносит cache_entries, зависящие
    /// хотя бы от одного из указанных file_paths (через reverse_index).
    /// Целевой клиент — code-index daemon: после `transaction.commit()`
    /// SQLite-индекса по batch'у FS-событий шлёт один POST со всеми изменёнными
    /// путями.
    #[serde(default)]
    pub file_paths: Option<Vec<String>>,
    /// Сокращённая форма для одного файла — эквивалентно `file_paths: [X]`.
    #[serde(default)]
    pub file_path: Option<String>,
}

pub async fn invalidate(
    State(state): State<AppState>,
    Json(req): Json<InvalidateRequest>,
) -> impl IntoResponse {
    let scope = pick_scope(&req.scope, &req.repo, &req.base);
    let removed = if req.all {
        let n = state.cache.len();
        state.cache.clear();
        n
    } else if let Some(paths) = req.file_paths.as_ref() {
        state.cache.invalidate_files(paths)
    } else if let Some(path) = req.file_path.as_ref() {
        state.cache.invalidate_files(std::slice::from_ref(path))
    } else if !scope.is_empty() {
        let prefix = Cache::scope_prefix(&state.server_alias, &scope);
        state.cache.invalidate_where(|k| k.starts_with(&prefix))
    } else if let Some(prefix) = req.key_prefix {
        state.cache.invalidate_where(|k| k.starts_with(&prefix))
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "укажите один из параметров: scope (синонимы repo/base), key_prefix, file_paths, file_path или all=true",
            })),
        )
            .into_response();
    };
    state.metrics.update_cache_size(state.cache.len());
    Json(json!({
        "removed": removed,
        "cache_size": state.cache.len(),
        "reverse_index_size": state.cache.reverse_index_size(),
    }))
    .into_response()
}

/// Один файл в payload `POST /mark-dirty`.
#[derive(Debug, Deserialize)]
pub struct MarkDirtyFile {
    pub path: String,
    /// observed mtime (unix-секунды), наблюдённый демоном на FS-событии.
    pub mtime: i64,
}

/// Ранний сигнал «пути грязные» от демона `code-index` — write-triggered ленивая
/// ревалидация (#1471). Шлётся ДО commit переразбора, в дополнение к
/// `POST /invalidate` после commit. Помечает `(repo, path)` грязными с observed
/// mtime; дальше прокси на чтении сверит его с индексным mtime из ответа serve.
#[derive(Debug, Deserialize)]
pub struct MarkDirtyRequest {
    /// scope (для cache-ci — алиас репо, `effective_alias()` пути в демоне).
    #[serde(default)]
    pub repo: Option<String>,
    /// Алиас имени `repo` — совместимость с внешними клиентами.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub files: Vec<MarkDirtyFile>,
}

pub async fn mark_dirty(
    State(state): State<AppState>,
    Json(req): Json<MarkDirtyRequest>,
) -> impl IntoResponse {
    let scope = req.repo.or(req.base).unwrap_or_default();
    for f in &req.files {
        state.dirty.mark(&scope, &f.path, f.mtime);
    }
    Json(json!({
        "marked": req.files.len(),
        "scope": scope,
        "dirty_size": state.dirty.len(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct FreezeRequest {
    /// Имя scope (для cache-ci — алиас репо). Если опущено или `""` —
    /// global-freeze (срабатывает на любой scope, включая отсутствующий).
    #[serde(default)]
    pub scope: Option<String>,
    /// Альтернативные имена для удобства внешних sidecar'ов — все означают
    /// то же, что и `scope`. Если переданы несколько — берётся первый
    /// непустой в порядке: `scope`, `repo`, `base`.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    /// Сколько секунд держать заморозку. Auto-snimается по истечении —
    /// safety на случай если внешний триггер забудет thaw.
    pub duration_seconds: u64,
}

pub async fn freeze(
    State(state): State<AppState>,
    Json(req): Json<FreezeRequest>,
) -> impl IntoResponse {
    let scope = pick_scope(&req.scope, &req.repo, &req.base);
    state
        .freeze
        .freeze(&scope, Duration::from_secs(req.duration_seconds));
    Json(json!({
        "frozen": true,
        "scope": scope,
        "duration_seconds": req.duration_seconds,
        "active": state.freeze.list_active(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ThawRequest {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
    /// Снять все заморозки разом.
    #[serde(default)]
    pub all: bool,
}

pub async fn thaw(
    State(state): State<AppState>,
    Json(req): Json<ThawRequest>,
) -> impl IntoResponse {
    if req.all {
        state.freeze.thaw_all();
        return Json(json!({"thawed": "all", "active": state.freeze.list_active()}))
            .into_response();
    }
    let scope = pick_scope(&req.scope, &req.repo, &req.base);
    state.freeze.thaw(&scope);
    Json(json!({
        "thawed": scope,
        "active": state.freeze.list_active(),
    }))
    .into_response()
}

pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.metrics.snapshot();
    Json(json!({
        "server_alias": state.server_alias,
        "backend_url": state.backend_url,
        "cache_size": snap.cache_size,
        "metrics": snap,
        "frozen_scopes": state.freeze.list_active(),
        "dirty_size": state.dirty.len(),
    }))
}

/// Выбор scope из трёх возможных полей запроса. Семантически они эквивалентны —
/// `scope` зарезервировано как родное имя, `repo`/`base` — алиасы для удобства
/// внешних sidecar'ов. Возвращает `""` если все три пусты — это global-режим.
fn pick_scope(
    scope: &Option<String>,
    repo: &Option<String>,
    base: &Option<String>,
) -> String {
    for v in [scope, repo, base] {
        if let Some(s) = v {
            if !s.is_empty() {
                return s.clone();
            }
        }
    }
    String::new()
}
