# Changelog

Все значимые изменения в этом проекте документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/), версионирование по [SemVer](https://semver.org/lang/ru/).

## [0.3.0] — 2026-05-14

### Изменено

- **Streamable HTTP — stateless mode by default.** Сервер переключён с `LocalSessionManager` на `NeverSessionManager` + `StreamableHttpServerConfig::with_stateful_mode(false)`. Заголовок `Mcp-Session-Id` от клиента **игнорируется**: любой клиент с любым (или пустым) session_id обслуживается без enforcement'а.
- **`with_json_response(true)`** — ответы отдаются как `Content-Type: application/json` вместо `text/event-stream`. Меньше overhead'а, нет SSE-framing'а — кеш-прокси не инициирует server-sent уведомлений, только отвечает на `tools/call`.

### Совместимость

- **Breaking — но узко**: в stateless-режиме rmcp **не поддерживает** `DELETE /mcp` (close session) и `GET /mcp` (standalone SSE stream). Эти методы вернут `405 Method Not Allowed`. Влияет только на клиентов, которые **явно** делают session-lifecycle (mainstream MCP-клиенты — VSCode `claude-code` extension, `claude` CLI, `mcp-cli` — не используют DELETE/GET, работают только через `POST /mcp` для `initialize` и `tools/call` → совместимость сохраняется).
- **`POST /mcp` (initialize, tools/list, tools/call)** работает идентично 0.2.x. Клиент при `initialize` получает 200 OK с `result.serverInfo` без `Mcp-Session-Id` в response header — это валидное поведение per spec MCP 2025-06-18.
- **Cache key format** не изменился: `{server_alias}|{scope}|{tool}|{sha256(normalize(args))}`. Все накопленные кеши совместимы с новой версией, очистка не требуется.

### Архитектура

- Stateful-режим в rmcp хранит `Mcp-Session-Id → state` (per-session worker'ы, SSE-кэш, in-flight router) **только в памяти**. Любой рестарт прокси (supervisor-respawn, ребут, ручной рестарт) или TTL-инвалидация по `SessionConfig::keep_alive` (default 5 минут) делают все ранее выданные session_id невалидными — следующий запрос клиента возвращает `404 Session not found`. Клиенты обязаны выполнять auto-reinit на 404 (повторный `initialize`, retry с новым session_id); часть существующих MCP-клиентов (включая VSCode extension `anthropic.claude-code` 2.1.141) этого не делает и требует ручного Reconnect — мажорная UX-проблема.
- Для кеш-прокси session_id концептуально избыточен: кеш-ключ строится по `(server_alias, scope, tool, sha256(args))`, **никакого state per-client** мы не храним. Stateless-режим устраняет источник нестабильности «404 после рестарта», сохраняя весь функционал (cache hits, TTL, single-flight, invalidation, freeze/thaw, reverse_index, metrics).
- Бонус: убрана session-cleanup нагрузка (фоновый таск `evict_expired_channels`), снижено потребление памяти при большом числе клиентов.

### Workspace

- Workspace version bumped 0.2.2 → **0.3.0** (minor — breaking узко для DELETE/GET).

## [0.2.2] — 2026-05-12

### Изменено

- **Release workflow: убран таргет `x86_64-apple-darwin`** (macOS Intel). Runner `macos-13` оказался deprecated и завис в очереди GitHub Actions >3 часов, блокируя публикацию релиза v0.2.1 (три из четырёх matrix-job'ов собрались успешно, но job `release` ждёт всю матрицу через `needs: build`). Apple Silicon (`aarch64-apple-darwin`) остаётся; x86_64 macOS на 2026 год — раритет, cross-compile усложнил бы CI без реального спроса. Workspace version bumped 0.2.1 → 0.2.2.

## [0.2.1] — 2026-05-12

### Добавлено

- **GitHub Actions release workflow** (`.github/workflows/release.yml`) — автоматическая сборка бинарников при push тега `v*` под четыре таргета: `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`. Артефакты (zip для Windows, tar.gz для остальных) с SHA-256 контрольными суммами прикрепляются к GitHub Release.

## [0.2.0] — 2026-05-12

### Добавлено

- **File-level точечная инвалидация** в cache-ci. Новые поля `file_paths: Vec<String>` и `file_path: String` в `POST /invalidate`. Сносят только cache_entries, зависящие от указанных файлов, не задевая соседних. См. `docs/audit-report.md` и `docs/implementation-plan.md`.
- **Reverse_index** (`file_path → cache_keys`) в `cache-core` (новый модуль `reverse_index.rs`). Наполняется при cache-fill из `_meta.dependent_files` ответа бэкенда (опциональное поле — обратная совместимость).
- **`CacheEntry::dependent_files`** — список файлов, на которых построен ответ. Используется при `evict_expired` и `invalidate_where` для чистки reverse_index.
- **Метод `Cache::insert_with_deps`** — записать ответ с явным списком зависимостей.
- **Метод `Cache::invalidate_files`** — точечная инвалидация по списку файлов через reverse_index.
- **Метод `Cache::reverse_index_size`** — размер индекса для метрик и наблюдения.
- **Per-scope `cacheable=false`** в `policy.rs` (новая структура `ScopePolicy`, секция `[scopes.<alias>]` в `cache_policy_*.toml`). Default — `cacheable=true` (поведение не меняется). Целевой use case — отключение кэша для federated репо при групповой работе одной правкой конфига без пересборки.
- **Prometheus text exposition format** для `GET /metrics` (text/plain; version=0.0.4). Метрики снабжены label `server="ci"`. Дополнительный gauge `cache_reverse_index_size`.
- **Новый endpoint `GET /metrics/json`** — JSON snapshot (старый формат `/metrics` для обратной совместимости с клиентами, читавшими JSON).
- **Метод `Metrics::to_prometheus_text(server_alias)`** в `cache-core`.
- **Поле `Metrics::reverse_index_size: AtomicUsize`** + `update_reverse_index_size`. Синхронизируется handler'ом `/metrics` перед отдачей.

### Изменено

- **`GET /metrics`** теперь возвращает Prometheus text exposition format вместо JSON. Старый JSON доступен через `GET /metrics/json` (обратная совместимость).
- **Response `/invalidate`** включает новое поле `reverse_index_size`.
- **`MetricsSnapshot`** дополнен полем `reverse_index_size: usize`.
- **Workspace version** bumped 0.1.0 → 0.2.0.

### Архитектура

- Точечная инвалидация задумана для **локальных репо**, где daemon code-index сам отслеживает FS-события и шлёт `POST /invalidate {file_paths}` (нужен code-index 0.9.1+). Для удалённых/federated сценариев, где сетевого моста к cache-ci нет, поведение fallback — `TTL` + опциональный внешний sidecar (`POST /freeze` на время крупной операции, `POST /thaw` после). Per-scope `[scopes.<alias>] cacheable=false` переключает конкретные репо в режим прямого passthrough — одно правление конфига, без пересборки.
- Бэкенды, ещё не присылающие `_meta.dependent_files` в ответах, продолжают работать как раньше — entries сохраняются без зависимостей, чистка по TTL. Обратная совместимость full.

### Следующие шаги (выходят за scope этого релиза)

- На стороне `code-index` daemon — добавить `_meta.dependent_files` в read-ответы data-tools (этап 2 архитектуры).
- На стороне `code-index` daemon — `POST /invalidate {file_paths}` после `transaction.commit()` SQLite по batch'у FS-событий (этап 3).
