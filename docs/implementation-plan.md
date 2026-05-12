# Implementation plan: Event-based cache invalidation

**Контекст:** реализация по согласованному [audit-report.md](audit-report.md). Архитектура — точечная инвалидация по `file_path` через reverse_index в cache-ci + `_meta.dependent_files` в каждом read-ответе daemon'а + расширение `/invalidate` параметром `file_paths`.

**Задействованные репозитории:**

- `mcp-services-cash` (workspace `cache-core` + `mcp-cache-ci`) — этот репо
- [code-index](https://github.com/Regsorm/code-index-mcp) (workspace `code-index-core` + `bsl-extension`)

---

## Этап 0 (Pre-requisite): `/metrics` endpoint в cache-ci

**Зачем:** снять baseline hit rate до миграции. Без цифр объективно оценить эффект невозможно.

**Файлы:**

- [crates/cache-core/src/metrics.rs](../crates/cache-core/src/metrics.rs) — добавить `to_prometheus_text(&self) -> String` (формат `cache_hits_total <N>` и т.д.).
- [crates/mcp-cache-ci/src/handlers.rs](../crates/mcp-cache-ci/src/handlers.rs) — новый handler `GET /metrics` (read AppState.metrics, отдать text/plain).
- [crates/mcp-cache-ci/src/main.rs](../crates/mcp-cache-ci/src/main.rs) — `.route("/metrics", get(handlers::metrics))`.

**Что наружу:** Prometheus text exposition format. Поля: `cache_hits_total`, `cache_misses_total`, `cache_bypass_total`, `cache_backend_errors_total`, `cache_entries_count`, `cache_backend_latency_micros_avg` (EMA).

**Релиз:** `mcp-services-cash` minor-bump.

**После релиза:** **неделя наблюдения** — снимать `/metrics` периодически (хоть кроном раз в час в текстовый лог), накопить baseline. Не блокирует следующий этап разработки, но блокирует roll-out этапа 3.

---

## Этап 1a: Cache-ci — reverse_index + параметр `file_paths` в `/invalidate`

**Зачем:** подготовить cache-ci к приёму событий от daemon. Обратно-совместимо: пока daemon не шлёт `_meta` и не вызывает `/invalidate {file_paths}` — никто этим не пользуется.

**Файлы (репо `mcp-services-cash`):**

- [crates/cache-core/src/cache.rs](../crates/cache-core/src/cache.rs):
  - `CacheEntry` дополнить опциональным `dependent_files: Vec<String>` (для двустороннего индекса).
  - Новый метод `insert_with_deps(key, payload, ttl, deps: Vec<String>)`.
  - Новый метод `invalidate_files(paths: &[String]) -> usize` — для каждого `path` достать из reverse_index `HashSet<String>` ключей, снести каждый из main cache, удалить path из reverse_index, вернуть число удалённых entries.
  - В `evict_expired()` дополнительно убирать каждый `dependent_file` уходящей entry из reverse_index (иначе утечка ключей в индексе).
- [crates/cache-core/src/reverse_index.rs](../crates/cache-core/src/reverse_index.rs) — **новый** модуль. `ReverseIndex(DashMap<String /*file_path*/, HashSet<String> /*cache_keys*/>)`. Атомарность через DashMap entry API.
- [crates/cache-core/src/proxy.rs](../crates/cache-core/src/proxy.rs) или wrapping слой — после получения backend response: парсить опциональное `_meta.dependent_files` из payload (через `serde_json::from_str::<Value>`), извлекать список, вызывать `cache.insert_with_deps(...)`. Поле `_meta` из ответа клиенту можно либо вырезать (чище), либо оставить (никто не сломается).
- [crates/mcp-cache-ci/src/handlers.rs](../crates/mcp-cache-ci/src/handlers.rs):
  - `InvalidateRequest` дополнить опциональным `file_paths: Option<Vec<String>>` (и одиночным `file_path: Option<String>` для удобства).
  - В `fn invalidate`: ветка «если переданы file_paths/file_path» → `state.cache.invalidate_files(...)`.

**Тесты (cache-core, unit):**

- `reverse_index_inserts_and_finds_keys`.
- `invalidate_files_drops_matched_entries_and_cleans_index`.
- `evict_expired_cleans_reverse_index`.
- `concurrent_invalidate_and_get_no_panic` (loom или 100 threads × 1000 iter).

**Тесты (mcp-cache-ci, integration через `tests/`):**

- `POST /invalidate {file_paths: [...]}` снижает `cache_entries_count` на ожидаемую величину.
- Старая форма `POST /invalidate {repo: ...}` продолжает работать.
- Cache-fill с `_meta.dependent_files` в backend response → reverse_index содержит указанные пути.

**Релиз:** `mcp-services-cash` minor-bump. Версия в `Cargo.toml` каждого крейта workspace.

**CHANGELOG / README / system-state.md:**

- `CHANGELOG.md` (новый или существующий): `### Добавлено` — file-level invalidate в /invalidate; `### Изменено` — CacheEntry хранит опциональный список зависимостей.
- `README.md`: пример вызова `POST /invalidate {file_paths: [...]}` + объяснение когда использовать (только если backend сам шлёт `_meta.dependent_files`).
- `~/.claude/rules/system-state.md` раздел про mcp-cache-ci: версия, новый endpoint, новое поле в /invalidate.

---

## Этап 1b: Cache-ci — per-scope `cacheable=false`, default=true

**Зачем:** заложить готовый механизм отключения кэша per scope. По умолчанию **не активирован** — поведение для всех scopes как сейчас (federated кешируется через TTL + cache-monitor). Целевой use case — переключение federated репо в режим passthrough при появлении групповой работы. Включается одним правлением `cache_policy_ci.toml` и рестартом cache-ci, без пересборки.

**Файлы (репо `mcp-services-cash`):**

- [config/cache_policy_ci.toml](../config/cache_policy_ci.toml) — расширить опциональным per-scope override:

  ```toml
  # Опциональные per-scope override. Если scope не упомянут — default cacheable=true.
  # Пример включения passthrough для federated репо при групповой работе:
  # [scopes.ut]
  # cacheable = false
  # [scopes.bp-ss]
  # cacheable = false
  # [scopes.bp-tdk]
  # cacheable = false
  # [scopes.zup]
  # cacheable = false
  ```

  В default-варианте секции `[scopes.*]` закомментированы — рабочее поведение не меняется.
- [crates/cache-core/src/policy.rs](../crates/cache-core/src/policy.rs) — расширить:
  - Новое поле `scopes: HashMap<String, ScopePolicy>` в структуре политики.
  - `ScopePolicy { cacheable: bool }` со значением по умолчанию `cacheable=true`.
  - Метод `is_cacheable(tool: &str, scope: &str) -> bool` — сначала смотрит scope, потом per-tool TTL, потом default.
- [crates/cache-core/src/proxy.rs](../crates/cache-core/src/proxy.rs) — на cache lookup: если `!policy.is_cacheable(tool, scope)` → пропустить lookup, форвардить напрямую, **не** сохранять ответ. Метрика `cache_bypass_total` с tag `reason="scope_disabled"`.

**Default-поведение в коде:** `cacheable=true` для любого scope, явно упомянутого или нет в `[scopes.*]`. Без правок конфига всё работает как до миграции.

**Тесты (cache-core, unit):**

- `is_cacheable_default_true_when_scope_not_in_config`.
- `is_cacheable_returns_false_when_scope_disabled`.
- `tools_with_short_ttl_still_respect_scope_disabled`.

**Тесты (mcp-cache-ci, integration):**

- Default config: запрос с `scope=ut` оседает в cache_entries как обычно.
- Override `[scopes.ut] cacheable=false`: запрос с `scope=ut` не оседает, `cache_bypass_total{reason="scope_disabled"}` инкрементируется. После убирания override — поведение возвращается к default.

**Релиз:** одним PR с этапом 1a (общая семантика «cache-ci учится точечно решать каждый ответ»). Один minor-bump `mcp-services-cash`.

**CHANGELOG / README:**

- `CHANGELOG.md` `### Добавлено`: per-scope `cacheable` override для тонкого контроля над кешированием отдельных репо/баз.
- `README.md`: документировать секцию `[scopes.<alias>]` с примером для federated репо.

---

## Этап 2: Daemon — `_meta.dependent_files` в read-ответах

**Зачем:** наполнить reverse_index в cache-ci. Без шага 3 — daemon ещё не дёргает invalidate, но cache-ci уже строит индекс. Можно сутки наблюдать `/metrics`, убедиться что reverse_index наполняется без проблем.

**Файлы (репо `code-index`):**

- [crates/code-index-core/src/mcp/tools.rs](../../MCP-Servers/code-index/crates/code-index-core/src/mcp/tools.rs) — основной модуль. Для каждого data-tool, возвращающего file-bound данные:
  - После сбора результата собрать `dependent_files: Vec<String>` — обычно это `DISTINCT file_path` из результатов либо явный path из args.
  - Завернуть response в JSON с дополнительным полем `_meta.dependent_files`.
- Возможно ввести общий хелпер `wrap_with_meta<T: Serialize>(result: T, deps: Vec<String>) -> Value`.

**Tools, в которые добавляется `_meta`:**

`search_function`, `search_class`, `find_symbol`, `get_function`, `get_class`, `get_imports`, `get_file_summary`, `get_callers`, `get_callees`, `grep_body`, `grep_code`, `grep_text`, `search_text`, `read_file`, `list_files`, и BSL-tools: `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`.

**Tools без `_meta`:** `health`, `get_stats`, `stat_file` (либо не зависят от файлов, либо tool уже non-cacheable, либо тривиальный single-file).

**Тесты (code-index, integration):**

- Для каждого tool — проверка наличия `_meta.dependent_files` в ответе и совпадение со списком из `result`.
- Тест `grep_body` без совпадений → `dependent_files = []`.

**Релиз:** `code-index` minor-bump (0.8.1 → 0.9.0).

**CHANGELOG / README / system-state.md (для code-index репо):**

- `CHANGELOG.md`: `### Добавлено` — все data-tools теперь возвращают `_meta.dependent_files`; ранее этого поля не было, клиенты должны игнорировать неизвестные поля в `_meta`.
- `README.md`: новый раздел про `_meta` в ответах, что это для cache-ci.
- `system-state.md` раздел про code-index: версия 0.9.0, новое поле в ответах.

**Pre-checkpoint между этапом 2 и 3:** сутки эксплуатации, проверка по `/metrics` что hit rate не упал (reverse_index ест память, но не должно ломать). Если что-то пошло не так — откат daemon к предыдущей версии.

---

## Этап 3: Daemon — `POST /invalidate {file_paths}` после commit SQLite

**Зачем:** замкнуть цепочку. После переиндексации batch'а файлов daemon шлёт один HTTP-запрос в cache-ci со списком изменённых path'ей.

**Файлы (репо `code-index`):**

- [crates/code-index-core/src/daemon_core/worker.rs](../../MCP-Servers/code-index/crates/code-index-core/src/daemon_core/worker.rs) — после commit SQLite-транзакции в обработке batch'а FS-событий: собрать список изменённых path'ей → передать в новый клиент.
- [crates/code-index-core/src/daemon_core/cache_client.rs](../../MCP-Servers/code-index/crates/code-index-core/src/daemon_core/cache_client.rs) — **новый**. `reqwest::Client`, метод `invalidate_files(targets: &[String], paths: &[String])`. Один POST `{file_paths: [...]}` на каждый target из `daemon.toml`. На failure — лог-предупреждение, не падать (TTL подстрахует).
- [daemon.toml](../../MCP-Servers/code-index/daemon.toml) — новая секция:
  ```toml
  [[cache_targets]]
  url = "http://127.0.0.1:8011"  # локальный cache-ci
  # url = "http://192.0.2.10:8011"  # remote Windows cache-ci, если применимо
  ```
- Конфиг парсится в [crates/code-index-core/src/daemon_core/config.rs](../../MCP-Servers/code-index/crates/code-index-core/src/daemon_core/config.rs).

**Critical (порядок):**

- Invalidate шлётся **после** `transaction.commit()` SQLite. Не до.
- Если commit упал → invalidate **не** шлётся (cache-ci продолжит отдавать старое до TTL — это приемлемо).
- Если commit прошёл, но invalidate не отправлен (сеть, cache-ci лежит) → TTL fallback, приемлемо.

**Federation (по решению из аудита, MVP a):** для federated репо новый механизм не активируется. Cache-monitor по `git pull` / DBConfigUpdate продолжает грубо чистить prefix (карточка #355).

**Тесты:**

- Integration: пишем тестовый файл → ждём debounce → проверяем что cache-ci получил POST `/invalidate {file_paths: ["..."]}`.
- e2e (вручную, чек-лист в README): открыть редактор, сохранить .bsl → grep_body на изменённую функцию мгновенно отдаёт свежий результат.

**Релиз:** `code-index` patch-bump (0.9.0 → 0.9.1) либо сразу 0.9.0 в одном релизе с этапом 2, если объём позволяет.

**CHANGELOG:** `### Добавлено` — daemon шлёт invalidate в cache-ci после переиндексации.

---

## Smoke-test после полного roll-out (этапы 1+2+3)

1. `curl http://127.0.0.1:8011/metrics` → `cache_entries_count` растёт под нагрузкой.
2. Открыть редактор, изменить функцию в `C:\RepoUT\src\...\SomeModule.bsl`.
3. Через 1.5-2 секунды (debounce + batch + invalidate) — `mcp__ci__grep_body(repo="ut", pattern="ИзменённыйТекст")` возвращает свежий результат.
4. `curl http://127.0.0.1:8011/metrics` → `cache_misses_total` инкрементировался на ту же функцию (свежий запрос), `cache_hits_total` для соседних функций в том же файле — не теряется.
5. Через час эксплуатации hit rate не упал vs baseline (по `/metrics`).

---

## Версионирование

- `mcp-services-cash`: SemVer, minor-bump (новая функциональность, обратно-совместимо).
- `code-index`: 0.8.1 → 0.9.0 (minor — новое поле в responses) → 0.9.1 (patch — invalidate-клиент, если разделять релизы).

---

## Дисциплина коммитов и push

Стандартный GitHub publish checklist:

1. Атомарные коммиты по слоям: `feat(cache-core)`, `feat(mcp-cache-ci)`, `feat(code-index-core)`, `test`, `docs`, финальный `release: vX.Y.Z`.
2. Перед каждым commit — аудит секретов по всей истории (grep на private IP, ключи, токены).
3. CHANGELOG.md + README.md обновляются **в том же релизном коммите**, не «потом».
4. system-state.md обновляется в том же коммите когда меняется версия системы.
5. `git push` — **только** по явному словесному указанию пользователя для каждого коммита отдельно. Не push'ить «по инерции» после первого согласованного push'а.
6. Tag аннотированный (`git tag -a vX.Y.Z -m "..."`), не lightweight.

---

## Карточка релиза после полного roll-out

Через `/save-investigation` (skill пишет в memory-mssql + rag-query PG):

- Что добавлено: file-level invalidate, dependent_files в metadata, /metrics endpoint.
- Версии: mcp-services-cash X.Y.Z + code-index 0.9.0.
- Команды повторного деплоя (на чужой машине): cross-compile под Linux (zigbuild) + scp на rag-VM + docker compose build/up для cache-ci.
- Baseline hit rate до / hit rate после (из /metrics).
- Известные ограничения: federation не покрывается file-level invalidate (по MVP a — TTL + cache-monitor).
- Связанные карточки: #355 (freeze/thaw + cache-monitor), #902 (sidecar architecture).

---

## Контекст для завтрашней сессии

При открытии завтра:

1. Прочитать [audit-report.md](audit-report.md) (раздел 5 — принятое решение, раздел 9 — federation a).
2. Прочитать этот файл.
3. Решить с какого этапа начинаем (0 или сразу 1).
4. Запустить subagent если этап объёмный (Sonnet через Agent для рутинной части), Opus для архитектурно важных мест.
