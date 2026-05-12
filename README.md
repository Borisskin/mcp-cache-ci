# mcp-cache-ci

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

## Эндпоинты

- `GET /health` — статус прокси + версия + cache_size.
- `GET /metrics` — Prometheus text exposition format (`text/plain; version=0.0.4`).
- `GET /metrics/json` — JSON snapshot (`MetricsSnapshot`).
- `GET /status` — расширенная информация (метрики + frozen_scopes).
- `POST /invalidate` — селективная инвалидация:
  - `all: bool` — снести весь кэш.
  - `repo: String` — снести по scope-prefix.
  - `key_prefix: String` — произвольный prefix для отладки.
  - `file_paths: Vec<String>` или `file_path: String` — **точечная инвалидация** по списку файлов через `reverse_index` (требует чтобы бэкенд присылал `_meta.dependent_files` в ответах — это умеет code-index ≥ 0.9.0).
- `POST /freeze` / `POST /thaw` — block-режим для scope/global на N секунд. Полезно если хочется остановить кэширование на время крупной операции (например, `git pull` всего репо) — внешний sidecar дёргает `/freeze` до и `/thaw` после.

Bypass-заголовок `X-Cache-Bypass: 1` обходит кэш для конкретного запроса.

## Per-scope override кэширования

В `config/cache_policy_ci.toml` можно отключить кэш для отдельного scope (репо):

```toml
[scopes.ut]
cacheable = false  # все запросы по repo=ut идут direct через бэкенд, в кэш не пишутся
```

Целевой use case — federated репо при групповой работе (когда инвалидация по событиям невозможна, но и stale-кэш недопустим). Default — `cacheable=true`.

## Совместимость

| code-index | Поведение |
|---|---|
| `≥ 0.9.0` | Полная event-based инвалидация. Бэкенд возвращает `_meta.dependent_files`, cache-ci регистрирует `cache_key → file_paths` в reverse_index. После переиндексации файла daemon шлёт `POST /invalidate {file_paths}` — точечный снос. |
| `< 0.9.0` | Только TTL fallback. `_meta.dependent_files` отсутствует → reverse_index пустой → точечная инвалидация не активна, кэш живёт по TTL. |

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
