# mcp-cache-ci

[Русская версия](README_RU.md)

Caching MCP proxy in front of the [code-index](https://github.com/Regsorm/code-index-mcp) server with event-based invalidation.

> Despite the `-ci` suffix, the binary is a **generic caching proxy for any MCP server that speaks Streamable HTTP**. You can deploy it in front of any backend (`code-index`, `rag-query`, `1c-router`, your own MCP server, …) by changing only the config file: `[backend].url` plus per-tool TTL/cacheable rules in `cache_policy_*.toml`. The `server_alias` constant (currently `"ci"`) gets baked into the cache key prefix — it stays the same regardless of which backend you wire up.

## Why

`code-index` answers MCP tool calls from a local SQLite index. Even with a fast SQLite, each call is still a network round-trip MCP → JSON → SQLite → JSON → MCP. In hot scenarios (repeated `grep_body`, `search_function` within a session) an in-memory cache in front of the backend turns tens of milliseconds into microseconds.

The main difference from a plain "TTL MCP cache" is **fine-grained invalidation by `file_path` via `reverse_index`**. When the code-index daemon re-indexes a file it sends `POST /invalidate {file_paths: [...]}`; the proxy evicts only the related entries — other cache hits stay alive. TTL remains as a safety net.

## Components

Cargo workspace with two crates:

- **`cache-core`** — shared core: config, TTL policy, in-memory cache on `DashMap`, single-flight, `reverse_index` (`cache_key → file_paths`), metrics, freeze/thaw, scope policy.
- **`mcp-cache-ci`** — HTTP MCP proxy binary in front of `code-index serve`. Listens on a configurable port (default 8011), forwards to the backend (default `http://127.0.0.1:8013/mcp` — typically you move `code-index serve` to 8013 so the proxy can take 8011).

## Build

```bash
cargo build --release
```

### On Windows (MSVC or GNU)

```bash
# MSVC (requires Visual Studio Build Tools 2022+)
cargo build --release --target x86_64-pc-windows-msvc

# GNU (requires MinGW-w64 in PATH)
cargo build --release --target x86_64-pc-windows-gnu
```

### Cross-compile to Linux from Windows

Via `cargo-zigbuild`:

```bash
cargo install cargo-zigbuild
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

You get a fully static ELF that can sit next to `code-index` on any Linux host.

## Run

```bash
mcp-cache-ci --config config/cache-ci.toml
```

Minimal `config/cache-ci.toml`:

```toml
bind_host = "127.0.0.1"
bind_port = 8011
default_ttl_seconds = 600
max_entries = 10000
max_memory_mb = 200

[backend]
url = "http://127.0.0.1:8013/mcp"   # code-index serve, usually moved to 8013
timeout_ms = 5000
```

Full configuration with all options — see `config/cache-ci.example.toml` and `config/cache_policy_ci.example.toml`.

## Endpoints

- `GET /health` — proxy status + version + `cache_size`.
- `GET /metrics` — Prometheus text exposition format (`text/plain; version=0.0.4`).
- `GET /metrics/json` — JSON snapshot (`MetricsSnapshot`).
- `GET /status` — extended info (metrics + `frozen_scopes`).
- `POST /invalidate` — selective invalidation:
  - `all: bool` — drop the entire cache.
  - `repo: String` — drop by scope-prefix.
  - `key_prefix: String` — arbitrary key prefix (handy for debugging).
  - `file_paths: Vec<String>` or `file_path: String` — **fine-grained invalidation** by file list via `reverse_index` (requires the backend to send `_meta.dependent_files` in responses — supported by code-index ≥ 0.9.0).
- `POST /freeze` / `POST /thaw` — block mode for a scope or globally for N seconds. Useful when you want to stop caching during a large operation (e.g. `git pull` over a whole repo) — an external sidecar can call `/freeze` before and `/thaw` after.

Bypass header `X-Cache-Bypass: 1` skips the cache for a single request.

## Per-scope cache override

In `config/cache_policy_ci.toml` you can disable caching for a specific scope (repo):

```toml
[scopes.ut]
cacheable = false  # all requests with repo=ut go directly to the backend, nothing is cached
```

Primary use case — federated repos under concurrent edits (when event-driven invalidation is unavailable but stale cache is also unacceptable). Default is `cacheable = true`.

## Compatibility

| code-index | Behaviour |
|---|---|
| `≥ 0.9.0` | Full event-based invalidation. Backend returns `_meta.dependent_files`, cache-ci registers `cache_key → file_paths` in `reverse_index`. After re-indexing a file the daemon sends `POST /invalidate {file_paths}` — targeted eviction. |
| `< 0.9.0` | TTL fallback only. `_meta.dependent_files` is missing → `reverse_index` stays empty → targeted invalidation is inactive, cache lives by TTL. |

## MCP transport: stateless mode

Since **0.3.0** the Streamable HTTP server runs in **stateless mode** (`StreamableHttpServerConfig::with_stateful_mode(false)` + `NeverSessionManager`). The `Mcp-Session-Id` header sent by a client is ignored — every request is served regardless of session state.

**Rationale.** This proxy's cache key is `{server_alias}|{scope}|{tool}|{sha256(args)}` — `session_id` was never part of it, so per-client state was unnecessary. In stateful mode rmcp keeps the session map in memory only: any proxy restart (manual, supervisor respawn) or TTL eviction (`SessionConfig::keep_alive`, 5 min default) invalidates every previously-issued session_id, so the next client request returns `404 Session not found`. Mainstream MCP clients (the VSCode `claude-code` extension, the `claude` CLI, the MCP SDKs) do **not** auto-reinit on 404 — the user has to hit "Reconnect" manually. Stateless removes this failure mode entirely.

**What still works:** `POST /mcp` with `initialize`, `tools/list`, `tools/call` — identical behaviour to 0.2.x. Cache hits, TTL, single-flight, invalidation, freeze/thaw, reverse_index, metrics — unchanged.

**What no longer works:** `DELETE /mcp` (close session) and `GET /mcp` (standalone SSE stream) return `405 Method Not Allowed`. These are session-lifecycle operations only — proxy clients that just call tools won't hit them.

## Metrics

Prometheus text format at `/metrics`:

```
cache_hits_total{server="ci"} 1234
cache_misses_total{server="ci"} 567
cache_bypass_total{server="ci"} 12
cache_backend_errors_total{server="ci"} 0
cache_entries_count{server="ci"} 845
cache_reverse_index_size{server="ci"} 2103
cache_backend_latency_micros_avg{server="ci"} 8421
```

Same snapshot in JSON: `GET /metrics/json`.

## Documentation

- [docs/audit-report.md](docs/audit-report.md) — audit of the migration from TTL-only to event-based invalidation.
- [docs/implementation-plan.md](docs/implementation-plan.md) — phase plan.
- [CHANGELOG.md](CHANGELOG.md) — changes per version.

## License

MIT
