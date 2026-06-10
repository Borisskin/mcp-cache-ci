# mcp-cache-ci

[English version](README.md)

> Несмотря на суффикс `-ci`, бинарник — **универсальный кеш-прокси перед любым MCP-сервером с транспортом Streamable HTTP**. Развернуть перед любым backend'ом (`code-index`, `rag-query`, `1c-router`, ваш собственный MCP-сервер, …) можно правкой только конфига: `[backend].url` плюс per-tool политика TTL/cacheable в `cache_policy_*.toml`. Константа `server_alias` (сейчас `"ci"`) попадает в префикс ключа кеша — она остаётся прежней независимо от подключённого backend'а.

Кэширующий MCP-прокси перед сервером [code-index](https://github.com/Regsorm/code-index-mcp) с событийной (event-based) инвалидацией.

## Зачем

`code-index` отдаёт ответы на tool-call'и из локального SQLite-индекса. Даже при быстром SQLite каждый запрос — это сетевой round-trip MCP → JSON → SQLite → JSON → MCP. На горячих сценариях (повторяющийся `grep_body`, `search_function` в одной сессии) кэш в RAM перед бэкендом даёт микросекундный ответ вместо десятков мс.

Главное отличие от обычного «MCP-кэша с TTL» — здесь точечная инвалидация по `file_path` через `reverse_index`. Когда daemon code-index переиндексировал файл — он шлёт `POST /invalidate {file_paths: [...]}`, прокси сносит только связанные entries, остальные cache hits сохраняются. TTL остаётся safety net.

## Состав

Cargo workspace из двух крейтов:

- **`cache-core`** — общее ядро: конфиг, политика TTL, in-memory cache на `DashMap`, single-flight, `reverse_index` (`cache_key → file_paths`), метрики, freeze/thaw, scope-policy.
- **`mcp-cache-ci`** — бинарник HTTP MCP-прокси перед `code-index serve`. Слушает на конфигурируемом порту (default 8011), форвардит на backend (default `http://127.0.0.1:8013/mcp` — обычно code-index переносят на 8013 чтобы прокси заняла 8011).

## Сборка

```bash
cargo build --release
```

### На Windows (MSVC или GNU)

```bash
# MSVC (требует Visual Studio Build Tools 2022+)
cargo build --release --target x86_64-pc-windows-msvc

# GNU (требует MinGW-w64 в PATH)
cargo build --release --target x86_64-pc-windows-gnu
```

### Cross-compile под Linux с Windows

Через `cargo-zigbuild`:

```bash
cargo install cargo-zigbuild
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Получится статический ELF, можно положить рядом с `code-index` на любом Linux-хосте.

## Запуск

```bash
mcp-cache-ci --config config/cache-ci.toml
```

Минимальный конфиг `config/cache-ci.toml`:

```toml
bind_host = "127.0.0.1"
bind_port = 8011
default_ttl_seconds = 600
max_entries = 10000
max_memory_mb = 200

[backend]
url = "http://127.0.0.1:8013/mcp"   # code-index serve, обычно переносят на 8013
timeout_ms = 5000
```

Полная конфигурация со всеми опциями — `config/cache-ci.example.toml` и `config/cache_policy_ci.example.toml`.

## Singleton-защита (опционально)

Флаг `--pid-file <путь>` (или env `MCP_CACHE_PID_FILE`) включает защиту от второго экземпляра. При старте прокси пишет свой PID в файл; если процесс с записанным PID уже жив — отказ старта, устаревший файл перезаписывается, при graceful shutdown файл удаляется (живость проверяется через `sysinfo`). Без флага lock не берётся.

Нужна на Windows под supervisor / планировщиком, где гонкой или из-за переиспользования PID можно стартовать второй экземпляр. В Docker не задавать — singleton там гарантирует сам контейнер (`container_name` + `restart` + bind порта), а stale-файл лишь мешал бы рестарту.

## Эндпоинты

- `GET /health` — статус прокси + версия + cache_size.
- `GET /metrics` — Prometheus text exposition format (`text/plain; version=0.0.4`).
- `GET /metrics/json` — JSON snapshot (`MetricsSnapshot`).
- `GET /status` — расширенная информация (метрики + frozen_scopes + dirty_size).
- `POST /mark-dirty` — ранний сигнал «пути грязные» от daemon code-index для **write-triggered ленивой ревалидации** (см. ниже). Тело: `{repo, files:[{path, mtime}]}`, шлётся на FS-событие *до* переразбора/commit, в дополнение к `/invalidate` после commit. Требует code-index ≥ 0.20.0.
- `POST /invalidate` — селективная инвалидация:
  - `all: bool` — снести весь кэш.
  - `repo: String` — снести по scope-prefix.
  - `key_prefix: String` — произвольный prefix для отладки.
  - `file_paths: Vec<String>` или `file_path: String` — **точечная инвалидация** по списку файлов через `reverse_index` (требует чтобы бэкенд присылал `_meta.dependent_files` в ответах — это умеет code-index ≥ 0.9.0). Это `_meta` — служебный канал serve↔cache-ci, и он снимается из ответов перед отдачей клиенту, включая `structuredContent._meta` extension-инструментов (с 0.4.2).
- `POST /freeze` / `POST /thaw` — block-режим для scope/global на N секунд. Полезно если хочется остановить кэширование на время крупной операции (например, `git pull` всего репо) — внешний sidecar дёргает `/freeze` до и `/thaw` после.

Bypass-заголовок `X-Cache-Bypass: 1` обходит кэш для конкретного запроса.

## Per-scope override кэширования

В `config/cache_policy_ci.toml` можно отключить кэш для отдельного scope (репо):

```toml
[scopes.ut]
cacheable = false  # все запросы по repo=ut идут direct через бэкенд, в кэш не пишутся
```

Целевой use case — federated репо при групповой работе (когда инвалидация по событиям невозможна, но и stale-кэш недопустим). Default — `cacheable=true`.

## Декомпозиция батчевых вызовов (mass-mode, с 0.5.0)

Батчевые tools/call code-index (`get_function`/`get_class` с `names: [...]`, `get_object_structure` с `full_names: [...]`) прокси режет на одиночные под-вызовы: каждый элемент проходит обычный конвейер (кэш, single-flight, freeze, ревалидация) и кэшируется **по объекту**, а не блобом на весь батч. Кэш-ключ под-вызова совпадает с ключом прямого одиночного вызова — кэш двусторонне общий: батч греет одиночные вызовы и наоборот.

- Хиты отдаются из кэша, в бэкенд параллельно (cap 16) уходят только промахи — partial-hit из коробки.
- Ответ `{results:[...]}` собирается строго в порядке имён запроса; битый элемент даёт `{error}` на своей позиции и не валит батч.
- Формат ответа идентичен mass-режиму самого serve — для клиента декомпозиция прозрачна.
- Для бэкендов без этих инструментов (например, деплой перед rag-query) — no-op.

## Write-triggered ленивая ревалидация

Начиная с **0.4.0**, поверх `POST /invalidate` (шлётся *после* commit переразбора, ~1.5 с после записи) прокси принимает ранний `POST /mark-dirty` (шлётся *до* переразбора) и ревалидирует лениво, сверяя mtime — отдаёт свежие данные сразу как индекс догнал диск, не дожидаясь TTL и не завися от доставки `/invalidate`.

Как работает:

1. Watcher демона ловит FS-событие и сразу шлёт `POST /mark-dirty {repo, files:[{path, mtime}]}` с наблюдённым mtime файла. Прокси помечает `(repo, path)` грязным (держит максимум observed mtime).
2. На чтении, чья запись зависит от грязного файла, прокси форвардит на backend (а не отдаёт из кэша) и сравнивает observed-mtime с индексным mtime из `_meta.file_mtimes` ответа serve.
3. Кэширует ответ и снимает флаг **только** когда `index_mtime >= observed` (индекс отразил диск). Иначе отдаёт ответ без запоминания и оставляет флаг.

**Strong-режим с бюджетом:** пока индекс отстаёт, прокси ретраит форвард до `revalidation_max_wait_ms` (default 2000), возвращая управление сразу как индекс догнал; по исчерпании бюджета — фолбэк на отдачу без кэширования. `revalidation_max_wait_ms = 0` → eventual (один форвард без ретраев).

**Федерация-safe:** «текущий» mtime приносит демон (co-located с файлами), поэтому прокси не обращается к файловой системе — работает и для федеративных репо, чьи файлы прокси не видит.

Ключи конфига в `[cache]`: `lazy_revalidation_enabled` (default `true`), `revalidation_max_wait_ms` (`2000`), `revalidation_retry_interval_ms` (`150`), `dirty_ttl_seconds` (`300`, страховочная чистка зависших dirty-флагов). `lazy_revalidation_enabled = false` → поведение 0.3.x.

## Совместимость

| code-index | Поведение |
|---|---|
| `≥ 0.9.0` | Полная event-based инвалидация. Бэкенд возвращает `_meta.dependent_files`, cache-ci регистрирует `cache_key → file_paths` в reverse_index. После переиндексации файла daemon шлёт `POST /invalidate {file_paths}` — точечный снос. |
| `< 0.9.0` | Только TTL fallback. `_meta.dependent_files` отсутствует → reverse_index пустой → точечная инвалидация не активна, кэш живёт по TTL. |

Для **write-triggered ленивой ревалидации** (0.4.0) бэкенд должен дополнительно отдавать `_meta.file_mtimes`, а daemon — слать `POST /mark-dirty`; и то и другое доступно в **code-index ≥ 0.20.0**. Со старым code-index ленивая ревалидация не активна, прокси откатывается на invalidate + TTL.

## MCP transport: stateless mode

Начиная с **0.3.0** Streamable HTTP-сервер работает в **stateless-режиме** (`StreamableHttpServerConfig::with_stateful_mode(false)` + `NeverSessionManager`). Заголовок `Mcp-Session-Id` от клиента **игнорируется** — каждый запрос обслуживается независимо от session state.

**Обоснование.** Ключ кэша этого прокси — `{server_alias}|{scope}|{tool}|{sha256(args)}` — `session_id` никогда не был его частью, per-client state нам не нужен. В stateful-режиме rmcp хранит session map **только в памяти**: любой рестарт прокси (ручной, supervisor-respawn) или TTL-инвалидация (`SessionConfig::keep_alive`, default 5 мин) делают все ранее выданные session_id невалидными — следующий запрос клиента возвращает `404 Session not found`. Массовые MCP-клиенты (VSCode-расширение `claude-code`, CLI `claude`, MCP-SDK) **не делают auto-reinit на 404** — пользователю приходится вручную жать «Reconnect». Stateless полностью устраняет этот класс отказов.

**Что продолжает работать:** `POST /mcp` с `initialize`, `tools/list`, `tools/call` — поведение идентично 0.2.x. Cache hits, TTL, single-flight, инвалидация, freeze/thaw, reverse_index, метрики — без изменений.

**Что больше не работает:** `DELETE /mcp` (закрытие сессии) и `GET /mcp` (standalone SSE stream) возвращают `405 Method Not Allowed`. Это операции session-lifecycle — клиенты, которые просто вызывают tools, их не используют.

## Метрики

Prometheus text format на `/metrics`:

```
cache_hits_total{server="ci"} 1234
cache_misses_total{server="ci"} 567
cache_bypass_total{server="ci"} 12
cache_backend_errors_total{server="ci"} 0
cache_entries_count{server="ci"} 845
cache_reverse_index_size{server="ci"} 2103
cache_backend_latency_micros_avg{server="ci"} 8421
```

Тот же snapshot в JSON: `GET /metrics/json`.

## Документация

- [docs/audit-report.md](docs/audit-report.md) — аудит миграции с TTL-only на event-based.
- [docs/implementation-plan.md](docs/implementation-plan.md) — план реализации этапов.
- [CHANGELOG.md](CHANGELOG.md) — список изменений по версиям.

## Лицензия

MIT
