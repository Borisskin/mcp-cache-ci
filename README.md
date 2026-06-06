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
- `GET /status` — extended info (metrics + `frozen_scopes` + `dirty_size`).
- `POST /mark-dirty` — early "paths are dirty" signal from the code-index daemon for **write-triggered lazy revalidation** (see below). Body: `{repo, files:[{path, mtime}]}`, sent on FS events *before* reparse/commit, in addition to `/invalidate` after commit. Requires code-index ≥ 0.20.0.
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

## Write-triggered lazy revalidation

Since **0.4.0**, on top of `POST /invalidate` (sent *after* the daemon commits a reindex, ~1.5 s after the write), the proxy accepts an early `POST /mark-dirty` (sent *before* reparse) and revalidates lazily by comparing mtimes — so it serves fresh data as soon as the index catches up, without waiting for TTL and without depending on `/invalidate` delivery.

How it works:

1. The daemon's watcher catches an FS event and immediately sends `POST /mark-dirty {repo, files:[{path, mtime}]}` with the observed disk mtime. The proxy marks `(repo, path)` dirty (keeping the max observed mtime).
2. On a read whose cached entry depends on a dirty file, the proxy forwards to the backend instead of serving the cached value, and compares the observed mtime against the index mtime from `_meta.file_mtimes` in the serve response.
3. It caches the response and clears the flag **only** when `index_mtime >= observed` (the index reflects disk). Otherwise it serves the response without caching and keeps the flag.

**Strong mode with a budget:** while the index is behind, the proxy retries the forward for up to `revalidation_max_wait_ms` (default 2000), returning as soon as the index catches up; on budget exhaustion it falls back to serving without caching. `revalidation_max_wait_ms = 0` → eventual (single forward, no retries).

**Federation-safe:** the "current" mtime is supplied by the daemon (co-located with the files), so the proxy never touches the filesystem — this works for federated repos whose files the proxy cannot see.

Config keys under `[cache]`: `lazy_revalidation_enabled` (default `true`), `revalidation_max_wait_ms` (`2000`), `revalidation_retry_interval_ms` (`150`), `dirty_ttl_seconds` (`300`, safety pruning of stuck dirty flags). Set `lazy_revalidation_enabled = false` for 0.3.x behaviour.

## Compatibility

| code-index | Behaviour |
|---|---|
| `≥ 0.9.0` | Full event-based invalidation. Backend returns `_meta.dependent_files`, cache-ci registers `cache_key → file_paths` in `reverse_index`. After re-indexing a file the daemon sends `POST /invalidate {file_paths}` — targeted eviction. |
| `< 0.9.0` | TTL fallback only. `_meta.dependent_files` is missing → `reverse_index` stays empty → targeted invalidation is inactive, cache lives by TTL. |

For **write-triggered lazy revalidation** (0.4.0) the backend must additionally emit `_meta.file_mtimes` and the daemon must send `POST /mark-dirty` — both available in **code-index ≥ 0.20.0**. With an older code-index lazy revalidation stays inactive and the proxy falls back to invalidate + TTL.

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
