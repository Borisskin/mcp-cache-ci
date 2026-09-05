# Changelog

Все значимые изменения в этом проекте документируются в этом файле.

Формат — [Keep a Changelog](https://keepachangelog.com/ru/1.0.0/), версионирование по [SemVer](https://semver.org/lang/ru/).

## [Не выпущено]

### Добавлено

- **`config/cache_policy_rag.toml` — боевая политика прокси перед `rag-query`.**
  До сих пор она существовала только на сервере и при переустановке машины не
  восстановилась бы. Дефолт в ней закрыт (`default_ttl_seconds = 0`), кэшируемые
  инструменты перечислены белым списком: чтение карточек, тел навыков и сессий —
  300 секунд, списки и поиск сессий — 60. Прежний открытый дефолт молча накрывал
  кэшем любой новый инструмент сервера — так под кэш попал `skill_search`, чей
  ответ зависит от состояния сессии, а сам вызов пишет журнал: из кэша вернулась
  бы копия прежней выдачи, а побочные записи не состоялись бы.

  Изменений в коде прокси нет, версия бинарника прежняя.

- **Сведения о продукте и версии внутри `mcp-cache-ci.exe`** (PR #2, Boris Sinitsyn).
  `crates/mcp-cache-ci/build.rs` встраивает в сборку под Windows название продукта,
  описание, владельца, знак охраны авторского права и номер версии из `Cargo.toml`.
  Версию установленного файла теперь видно в его свойствах — чтобы её узнать, больше
  не нужно запускать службу и читать журнал. Сборки под Linux и macOS шаг пропускают:
  в `build.rs` стоит условие `CARGO_CFG_TARGET_OS == "windows"`.

### Исправлено

- **Журнал при перенаправлении вывода — в stdout и без цветовых последовательностей**
  (PR #1, Boris Sinitsyn). Раньше записи всегда шли в stderr и всегда с ANSI-оформлением:
  в файле журнала копились управляющие символы вида `[2m[32m INFO`, а планировщик
  заданий Windows, перенаправляющий stderr средствами PowerShell, вдобавок оборачивал
  строки в записи NativeCommandError. Теперь поток выбирается по признаку терминала:
  в интерактивном терминале — прежний цветной stderr, при перенаправлении — чистый
  stdout без оформления. Расположение записей это не меняет: супервизор пишет stdout и
  stderr дочернего процесса в один файл, а в контейнере оба потока забирает `docker logs`.
  Сообщение о неустранимой ошибке запуска по-прежнему уходит в stderr — его печатает
  среда выполнения Rust, а не наш приёмник журнала.

## [0.6.0] — 2026-07-09

**Имя прокси и область кэширования переехали из констант компиляции в конфиг.**

### Изменено (ЛОМАЮЩЕЕ)

- **`[server].alias` — новый ОБЯЗАТЕЛЬНЫЙ ключ конфига.** Раньше имя прокси было
  константой `SERVER_ALIAS` в `main.rs`. Один бинарник, развёрнутый как несколько
  служб, отвечал всем одинаковым именем. Значения по умолчанию нет намеренно:
  пропуск ключа роняет запуск с ошибкой разбора, а не подставляет тихо чужое имя.
  Конфиги без `alias` не стартуют — добавьте ключ перед обновлением.
- **`[server].scope_args` — список имён аргументов, по которым кэш делится на
  области** (было константой `SCOPE_ARG_NAME`). Проверяются по порядку, берётся
  первое найденное строковое поле. `["repo"]` — code-index, `["base"]` — 1c-router,
  `[]` или отсутствие — бэкенд без разделения. По умолчанию пустой список.
- **`POST /invalidate` принимает родное имя `scope`.** `repo` и `base` сохранены
  как устаревшие синонимы (разбор через тот же `pick_scope`, что у `/freeze`),
  старые вызывающие не ломаются.

### Исправлено

- **Выборочный сброс кэша по области не работал у прокси перед `1c-router`.**
  Инструменты 1С принимают `base`, а зашитая константа искала `repo` → область
  в ключе оставалась пустой, и `POST /invalidate {"base": "..."}` возвращал
  `removed: 0` при непустом кэше. Воспроизведено на живом `:8010` до правки.
- **Метки Prometheus обоих прокси схлопывались.** Оба отдавали `server="ci"`,
  хотя `metrics.rs` рассчитан на различение экземпляров. Теперь `server="1c"`
  и `server="rag"`.
- **Текст `instructions` называл алиас именем бэкенда.** Было «Кэширующий прокси
  перед бэкендом `'ci'`», хотя `ci` — имя самого прокси. Стало «Кэширующий
  прокси `'ci'`».

### Тесты

- 84 passed, 0 failed. Локальный дым на живом `bsl-indexer:8011` (`scope_args=["repo"]`)
  и на `1c-router` через SSH-туннель (`scope_args=["base"]`): попадание в кэш,
  `/invalidate {scope}` сносит записи, чужая область не задевается, при `scope_args=[]`
  область остаётся пустой.
- Боевая проверка на ВМ после раскатки `0.6.0`: `/health` отдаёт `1c` и `rag`,
  `/invalidate {"scope":"smaks-ut"}` на `:8010` сносит записи (было `removed: 0`),
  метки метрик разошлись, сквозной `tools/call` проходит через оба прокси.

## [0.5.0] — 2026-06-10

**Декомпозиция батчевых tools/call: per-object кэш для mass-mode code-index.**

### Добавлено

- **Модуль `cache-core::batch` — батч режется на одиночные под-вызовы.** Батчевые вызовы mass-mode (`get_function`/`get_class` с `names[]`, `get_object_structure` с `full_names[]`) раньше кэшировались/форвардились как единое целое — ключ строился по всему батчу, per-object кэш не работал. Теперь каждый элемент проходит штатный `CacheProxy::handle` (кэш, single-flight, freeze, dirty-ревалидация, метрики) и кэшируется ПО ОБЪЕКТУ. Кэш-ключ под-вызова байт-в-байт совпадает с ключом прямого одиночного вызова того же инструмента → кэш двусторонне общий: батч греет одиночные вызовы и наоборот. Хиты отдаются из кэша мгновенно; в serve ПАРАЛЛЕЛЬНО (`buffered`, cap 16) уходят только промахи — одиночными вызовами, не под-батчем (одиночный ответ несёт `_meta.dependent_files` per-object для reverse-index-инвалидации, батчевый ответ serve их срезает). Порядок `results` = порядку имён; нестроковый элемент → `{error}` на своей позиции; пустой массив → `{"results":[]}` без обращения к бэкенду; `Frozen` — ошибка всего батча (freeze scope-глобальный). Формат элементов зеркалит mass-режим serve: core-tools — `{result, hint?}`, extension `get_object_structure` — голая структура (`unwrap_result`). Для бэкендов без этих инструментов (`mcp-cache-rag` и др.) — no-op, поведение прокси не меняется.

### Тесты

- 12 новых тестов `batch::` (детект, порядок, partial-hit, двусторонний прогрев кэша, нестроковый элемент, пустой список, frozen, unwrap_result для extension-форм). 84 passed, 0 failed. Смок на живом `:8011`: батч×4 с одним прогретым объектом — `cache_hits +1`, в serve ушли только 3 промаха; повторный батч — `cache_hits +4`, `cache_misses +0`; порядок сохранён; одиночные вызовы и не-батчевые инструменты без изменений.

### Совместимость

- Workspace version 0.4.2 → **0.5.0** (minor — новая функциональность, формат запросов/ответов клиента не менялся). Локальный `mcp-cache-ci` под mcp-supervisor пересобран и задеплоен. ВМ-инстансы (`mcp-cache-ci`, `mcp-cache-rag` на rag) пересобрать отдельно — до этого батчи там кэшируются целиком (прежнее поведение, ничего не ломается).

## [0.4.2] — 2026-06-09

**Служебное поле `_meta` больше не доезжает до клиента — ни через `content`, ни через `structuredContent`.**

### Исправлено

- **`_meta` снимается из ответов клиенту во всех путях.** `_meta` (`dependent_files` / `file_mtimes`) — служебный канал serve↔cache-ci: deps идут в `reverse_index`, mtimes — во write-triggered ревалидацию (#1471). Модели это поле не нужно и раздувает контекст. `strip_meta` снимает его в `forward_and_cache`, reval-loop и `forward_no_cache` перед кэшированием и отдачей; deps/mtimes извлекаются из сырого payload ДО strip — инвалидация не страдает.
- **Очистка `structuredContent._meta` (закрывает дыру для extension-tools).** BSL-extension инструменты code-index serve (`get_object_structure`/`get_object_profile`, `bsl_sql`, `get_data_links` и др.) отдают rmcp `structuredContent = {_meta, result}` ПОМИМО `content[*].text`. Прежняя ветка чистила только `content[*].text`, и `_meta` доезжал до клиента через `structuredContent`. Добавлена ветка очистки `structuredContent._meta`. Обнаружено на живом ut-test (`get_object_structure` отдавал `_meta`).

### Тесты

- Юнит-тест `strip_meta_removes_meta_from_structured_content` на РЕАЛЬНОЙ форме extension-ответа (content + structuredContent). 72 теста passed. Смок на живом: `get_object_structure`/`get_object_profile` — `_meta` нигде (miss и hit), `structuredContent.result` цел; core-tools (`find_symbol`/`get_callers`) без регрессии.

### Совместимость

- Workspace version 0.4.1 → **0.4.2** (patch — очистка ответов, аддитивно). Локальный `mcp-cache-ci` под mcp-supervisor пересобран и задеплоен. ВМ-инстансы (`mcp-cache-ci`, `mcp-cache-rag` на rag) пересобрать отдельно — там без фикса `_meta` extension-tools доезжает через federation-кэш.

## [0.4.1] — 2026-06-09

### Добавлено

- **Opt-in singleton PID-lock** через CLI `--pid-file` / env `MCP_CACHE_PID_FILE`
  (sysinfo 0.32; эталон — code-index `daemon_core/lock.rs`). При заданном пути: если
  процесс с записанным PID жив — отказ старта; устаревший PID-файл перезаписывается;
  RAII Drop удаляет файл при graceful shutdown. Если путь не задан — lock не берётся
  (для Docker, где singleton гарантирует контейнер: `container_name` + `restart` + bind порта).
- Развёрнуто синхронно на 3 инстансах: локальный `mcp-cache-ci` под mcp-supervisor
  (lock **активен** — `MCP_CACHE_PID_FILE` задан в `services.json`, PID-файл создаётся),
  на ВМ rag `mcp-cache-ci` и `mcp-cache-rag` (lock dormant — env не задан).

### Совместимость

- **Аддитивно, не breaking.** Без `--pid-file`/`MCP_CACHE_PID_FILE` поведение 0.4.0 без изменений.
- Workspace version 0.4.0 → **0.4.1** (patch — аддитивная фича). Образы на ВМ
  (`mcp-cache-ci:0.4.0`, `mcp-cache-rag:0.3.0`) пересобраны с этим бинарником, но
  тег образа и `/health` сообщают прежнюю версию до следующей пересборки.

## [0.4.0] — 2026-06-06

### Добавлено

- **Write-triggered ленивая ревалидация кэша (#1471).** Прокси научился отдавать свежие данные сразу как индекс догнал диск, не дожидаясь TTL и не завися от доставки `POST /invalidate`.
  - **Новый эндпоинт `POST /mark-dirty`** — ранний сигнал от демона `code-index` при FS-событии (ДО переразбора/commit), в дополнение к `POST /invalidate` после commit. Тело: `{repo, files:[{path, mtime}]}`, где `mtime` — observed (наблюдённый на диске) unix-mtime. Прокси помечает `(repo, path)` грязными.
  - **Сверка mtime перед запоминанием.** На чтении, чья запись зависит от грязного файла, прокси форвардит на backend и сравнивает observed-mtime с индексным mtime из `_meta.file_mtimes` ответа serve. Кэширует ответ и снимает флаг **только** когда `index_mtime >= observed` (индекс реально отразил диск). Иначе — не кэширует. Это закрывает гонку «перекеширования старья» в окне переразбора.
  - **Strong-режим с бюджетом.** В окне переразбора прокси ретраит форвард до `revalidation_max_wait_ms` (дефолт 2000 мс), возвращая управление сразу как индекс догнал; по исчерпании бюджета — мягкий фолбэк (отдать ответ без запоминания, флаг оставить). `revalidation_max_wait_ms = 0` → eventual (один форвард без ретраев).
  - **Федерация-safe.** «Текущий» mtime приносит демон (он co-located с файлами) — прокси не обращается к файловой системе, поэтому механизм работает и для федеративных репо (`ut`/`bp-*`/`zup` на ВМ), чьи файлы прокси не видит.
- **`dirty_size`** в ответах `GET /status`.

### Изменено

- **Конфиг `[cache]`** — новые ключи: `lazy_revalidation_enabled` (default `true`), `revalidation_max_wait_ms` (default `2000`), `revalidation_retry_interval_ms` (default `150`), `dirty_ttl_seconds` (default `300`, страховочная чистка зависших dirty-флагов в фоновой эвикции). При `lazy_revalidation_enabled = false` — поведение 0.3.x без изменений.

### Совместимость

- **Аддитивно, не breaking.** Cache key format не изменился, накопленные кеши совместимы. `POST /mark-dirty` — best-effort: демон старее не шлёт его, прокси без поддержки вернёт 404 (демон проглатывает). Если в ответе serve нет `_meta.file_mtimes` (serve старее `code-index` без поддержки поля) — прокси не может сверить mtime и продолжает форвардить, пока путь грязный (безопасная деградация, не баг). Синхронный деплой serve + cache-ci + daemon рекомендован, но не обязателен.
- Workspace version bumped 0.3.0 → **0.4.0** (minor — новая функциональность).

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
