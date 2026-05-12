# Audit report: TTL-only кэш `mcp-cache-ci` → событийная инвалидация по `file_path`

**Статус:** фаза 1, только отчёт. Код не меняется. Реализация — следующей итерацией после согласования.

**Дата:** 2026-05-12.

**Контекст:** внешнее ТЗ предлагает мигрировать `mcp-cache-ci` с TTL-only на «blake3 + FS watcher в cache-ci + точечная инвалидация по `dependent_files` + Prometheus + feature flag». При ресёрсе обнаружено, что часть предложения **уже реализована** в смежных компонентах (FS watcher и хэширование живут в `code-index` daemon), часть **не нужна** (blake3 не даёт ROI при уже работающем SHA256), а корневой gap — не в TTL и не в алгоритме хэширования, а в **отсутствии привязки `cached_response → файлы`** между двумя сервисами.

---

## 1. Текущая архитектура `mcp-cache-ci` (факты)

**Workspace** (`Cargo.toml`): 3 крейта — `cache-core` (общее ядро для прокси), `mcp-cache-ci` (прокси перед code-index), `cache-monitor` (sidecar мониторинга крупных событий — git pull всего репо).

**Cache key** ([crates/cache-core/src/cache.rs](C:/Tools/mcp-services-cash/crates/cache-core/src/cache.rs):67-72):

    format!("{server_alias}|{scope}|{tool}|{hash_hex}")

- `server_alias` = `"ci"` для cache-ci.
- `scope` = `args.repo` для cache-ci. Пустая строка для tool-вызовов без scope.
- `hash_hex` = `sha256(normalize_args(args))`, где `normalize_args` ([cache.rs:148-170](C:/Tools/mcp-services-cash/crates/cache-core/src/cache.rs#L148-L170)) рекурсивно сортирует ключи JSON-объектов — нечувствительность к порядку полей в args.

**CacheEntry** ([cache.rs:22-32](C:/Tools/mcp-services-cash/crates/cache-core/src/cache.rs#L22-L32)):

    pub struct CacheEntry {
        pub payload: Arc<String>,
        pub expires_at: Instant,
        pub recorded_at: Instant,
    }

**Корневой gap:** привязки entry → список файлов, на которых построен этот payload, в структуре entry **нет**. Хранилище — `DashMap<String, CacheEntry>`, in-process, без персистентности.

**TTL** ([config/cache-ci.toml](C:/Tools/mcp-services-cash/config/cache-ci.toml), [config/cache_policy_ci.toml](C:/Tools/mcp-services-cash/config/cache_policy_ci.toml)):

- Дефолт: 600 секунд (10 минут).
- Per-tool override: `search_function`/`search_class`/`find_symbol`/`get_function`/`get_class`/`get_imports` = 3600s (1 час); `get_file_summary`/`get_callers`/`get_callees` = 1800s (30 мин); `grep_body`/`search_text` = 600s; `get_stats` = 60s; `health` = non-cacheable.
- Эвикция фоновая раз в 60s + lazy при `get`.

**Инвалидация** ([crates/mcp-cache-ci/src/handlers.rs:61-85](C:/Tools/mcp-services-cash/crates/mcp-cache-ci/src/handlers.rs#L61-L85)):

    pub async fn invalidate(State(state), Json(req): Json<InvalidateRequest>) -> impl IntoResponse {
        let removed = if req.all { ... clear ... }
        else if let Some(scope) = req.repo.or(req.base) {
            let prefix = Cache::scope_prefix(&state.server_alias, scope);
            state.cache.invalidate_where(|k| k.starts_with(&prefix))
        } else if let Some(prefix) = req.key_prefix {
            state.cache.invalidate_where(|k| k.starts_with(&prefix))
        } else { ... 400 ... };
        ...
    }

Только **prefix-инвалидация** (по `repo`/`base`/произвольному prefix) или `all` (снос всего). Точечной по `file_path` нет.

**Заморозка** ([crates/cache-core/src/freeze.rs](C:/Tools/mcp-services-cash/crates/cache-core/src/freeze.rs)): отдельный механизм freeze/thaw по scope — возвращает JSON-RPC error `Frozen` на любые `tools/call` к замороженному scope. Snapshot через `arc_swap::ArcSwap`. **Спроектирована под редкие крупные события** (DBConfigUpdate из eventlog 1С). Вызывается `cache-monitor`'ом, не клиентом. В новой системе не трогаем.

**Метрики** ([crates/cache-core/src/metrics.rs](C:/Tools/mcp-services-cash/crates/cache-core/src/metrics.rs)): atomic-счётчики `cache_hits`, `cache_misses`, `bypass`, `backend_errors`, `cache_size`, EMA `avg_backend_latency_micros` (CAS-loop, ALPHA=0.1). `/metrics` endpoint наружу не expose — текущий hit rate объективно неизвестен.

**Конфигурация** (репозиторный дефолт `config/cache-ci.toml`): `bind_port=8013`, `backend.url=http://127.0.0.1:8011/mcp`, `timeout_ms=5000`, `max_entries=10000`, `max_memory_mb=200`, `default_ttl_seconds=600`, `evict_interval_seconds=60`. Production-bind на разных машинах может отличаться (на rag-VM cache-ci слушает 8011; реальные значения смотреть в production-конфиге, не в репо).

---

## 2. Текущая архитектура `code-index` daemon (факты, релевантные для новой архитектуры)

- **FS watcher уже есть** — [crates/code-index-core/src/watcher.rs](C:/MCP-Servers/code-index/crates/code-index-core/src/watcher.rs), 323 строки. `notify` v7 (cross-platform). Debounce 1500ms (окно молчания после последнего FS-события), batch 2000ms (макс. окно сборки batch'а). Жёсткие игнор-листы (`.git`, `target`, `node_modules`, `.code-index`) + гибкие из `daemon.toml`.
- **Хэширование уже есть** — [hasher.rs](C:/MCP-Servers/code-index/crates/code-index-core/src/indexer/hasher.rs). **SHA-256, не blake3.** Три уровня: `content_hash` (весь файл), `ast_hash` (AST), `node_hash` (per function/class).
- **Таблица `files`** ([storage/schema.rs](C:/MCP-Servers/code-index/crates/code-index-core/src/storage/schema.rs)): `path`, `content_hash`, `ast_hash`, `language`, `lines_total`, `indexed_at`, `mtime`, `file_size`. Миграция v3 — mtime+size. v4 — таблица `file_contents` с zstd-сжатым содержимым.
- **Привязка «данные индекса → файл» в SQLite полная**: каждая data-таблица (`functions`, `classes`, `file_contents`, `imports`, `proc_call_graph`) несёт колонку `file_path` или `file_id`. Daemon уже использует её при формировании ответов — каждый элемент результата `search_function`/`grep_body`/`find_symbol` содержит `file_path`.
- **IPC daemon ↔ serve**: HTTP loopback. Daemon expose `/health`, `/path-status`, `/reload`, `/stop`. **Эндпоинта для отправки cache-invalidate в сторону cache-ci сейчас нет** — это и есть новый код в реализации.
- **Re-index** ([worker.rs::apply_event](C:/MCP-Servers/code-index/crates/code-index-core/src/daemon_core/worker.rs)): по FS-событию пересчитывает SHA256, при отличии — DELETE+INSERT в SQLite транзакцией.
- **Federation (rc6+)**: cross-node инвалидации нет.

---

## 3. Метрики и неизвестные

- **Размер индекса** (через `get_stats()` без аргумента): 26 подключённых репо, общее число файлов ~280K (bp-ss ~94K, bp-tdk ~90K, ut ~57K, zup ~40K, остальные мельче).
- **Реальный hit rate неизвестен.** Метрики в памяти есть, наружу не отдаются. Логов hit/miss нет.
- **Горячие tools** (по картине использования модели): `grep_body`, `search_function`, `get_file_summary`, `search_class`, `find_symbol`.
- **Latency cache hit vs miss vs прямой SQLite — не замерено**, требует benchmark.

---

## 4. Слабые места (root cause)

1. **Нет канала событий daemon → cache-ci.** Daemon знает о file change через debounce 1.5s. Cache-ci узнаёт только по истечении TTL (до 1 часа для search_function). Это и есть «stale read window».
2. **Нет привязки cache_entry → файлы в cache-ci.** Даже при наличии канала событий невозможно сказать, какие именно entries инвалидировать. Сейчас можно только prefix-снос (по repo или repo+tool), что катастрофично для частых событий dev-сессии: одно сохранение файла снесёт все cached responses по этому tool в этом репо.
3. **Нет observable hit/miss.** Atomic-счётчики живут только в RAM, теряются при рестарте, наружу не отдаются. Невозможно объективно оценить ни текущее состояние, ни эффект миграции.

---

## 5. Принятое архитектурное решение

Точечная инвалидация по `file_path` через **reverse-index в cache-ci**. Два независимых канала между daemon и cache-ci.

### Канал A — `_meta.dependent_files` в каждом read-ответе daemon'а (часто, попутно с трафиком)

Daemon на любой data-tool (`search_function`, `grep_body`, `get_function`, `get_file_summary`, `find_symbol`, `read_file`, `search_class`, `get_callers`, и т.д.) добавляет в JSON-ответ поле metadata:

    {
      "result": [ ... ],
      "_meta": { "dependent_files": ["src/X.bsl", "src/Y.bsl", "src/Z.bsl"] }
    }

Список собирается из тех же SELECT'ов, что уже исполняются для формирования `result` — все data-таблицы несут `file_path` в каждой строке. Стоимость на стороне daemon близка к нулю: `SELECT DISTINCT file_path FROM <те же таблицы> WHERE <те же условия>` или сбор путей из уже подготовленных результатов.

Cache-ci при cache-fill читает `_meta.dependent_files`, для каждого пути делает `reverse_index[path].insert(cache_key)`. Поле `_meta` либо вырезается из payload перед отдачей клиенту, либо игнорируется клиентом — поведение клиентов не меняется.

### Канал B — `POST /invalidate {file_paths: [...]}` от daemon к cache-ci (редко, по FS event после переиндексации)

После применения batch'а изменений в SQLite (commit транзакции) daemon шлёт один HTTP-запрос cache-ci со списком файлов, которые изменились в текущей пачке. Cache-ci через reverse_index находит затронутые `cache_keys`, сносит их из main cache, удаляет `file_paths` из reverse_index. Соседние entries, построенные на других файлах, остаются живыми.

Существующая форма `POST /invalidate {repo?, base?, key_prefix?, all?}` расширяется новым параметром `file_paths` (либо одиночным `file_path` для совместимости).

### TTL остаётся как safety net

Если событие потерялось (буфер `ReadDirectoryChangesW` переполнен при массовом git checkout, daemon упал между SQLite-commit и invalidate-запросом), entry через 600s/3600s протухнет по дефолтному механизму. Дефолтные значения не меняются.

### freeze + prefix-invalidate не трогаем

Они остаются за `cache-monitor`'ом для редких крупных событий (DBConfigUpdate из eventlog 1С, git pull всего репо) — отдельный класс задач, спроектированный и работающий (карточка #355). Новый file-level invalidate — **третий режим**, не заменяющий два предыдущих.

### Versioning daemon ↔ cache-ci

Изменение обратно-совместимо: если daemon старее и не шлёт `_meta.dependent_files`, cache-ci сохраняет entry без зависимостей и работает как сейчас (только по TTL). Один if в обработчике ответа, отдельного feature flag не требует.

---

## 6. Pre-requisite перед миграцией: `/metrics` endpoint

Перед началом реализации — добавить `/metrics` endpoint в `mcp-cache-ci` и снимать hit rate **в течение недели** до миграции. Atomic-счётчики уже есть в [crates/cache-core/src/metrics.rs](C:/Tools/mcp-services-cash/crates/cache-core/src/metrics.rs), нужно только тонкий axum-handler в `crates/mcp-cache-ci/src/handlers.rs` (Prometheus-формат либо JSON). Десятки строк кода.

Без этой baseline-цифры объективно оценить эффект миграции невозможно. Эта работа не блокирует архитектурное решение и не относится к самой миграции — это её обязательный pre-step.

---

## 7. Что в ТЗ оказалось неактуально

- **Blake3.** Daemon уже на SHA256 (content + AST + node). Переход даст маргинальный CPU-выигрыш, но потребует миграцию schema v5 + пересчёт хэшей всех 280K файлов. ROI плохой.
- **Новый FS watcher в cache-ci.** Daemon уже его имеет; два watcher'а на одни и те же inotify/ReadDirectoryChangesW-события создадут races без выигрыша.
- **In-memory map file→hash + сверка хэшей при cache lookup в cache-ci.** Не нужна — событийная модель достаточна. Hash-snapshot имеет смысл только при недоверии каналу событий, что вырождается в существующий TTL fallback.
- **Hash-based cache key derivation.** Текущий `sha256(normalize_args(args))` стабилен; зависимость entry от файлов отслеживается отдельной структурой `reverse_index`, а не зашита в ключ.
- **Feature flag `CACHE_MODE=ttl_only|hash_based`.** Изменение обратно-совместимо: cache-ci продолжает работать по TTL если daemon не шлёт `_meta.dependent_files` (старые daemon'ы, smoke-окружения). Отдельный флаг не нужен — поведение деградирует автоматически.
- **Prometheus endpoint /metrics.** Нужен, но не блокирующий миграцию. Внутренние счётчики уже есть — отдать их через тонкий handler можно отдельно (рекомендуется до миграции, чтобы снять hit rate baseline).
- **Load test 1000 req/s 10 минут.** Текущий профиль — десятки req/s, не тысячи. Stress сверх профиля даст data point, но не является блокирующим acceptance criteria.

---

## 8. Последовательность инвалидации

| Шаг | Кто | Что делает | Статус сегодня |
|---|---|---|---|
| 1 | daemon | Получает FS-сигнал об изменении файла (notify watcher, debounce 1.5s) | Уже работает |
| 2 | daemon | Перечитывает файл, пересчитывает SHA256, обновляет SQLite (DELETE+INSERT), батчем по `batch_ms=2000` | Уже работает |
| 3 | daemon | **После commit SQLite-транзакции** дёргает `POST /invalidate {file_paths: [<вся пачка>]}` на cache-ci одним запросом | Новое |
| 4 | cache-ci | По `reverse_index` находит затронутые `cache_keys`, сносит их из main cache, удаляет `file_paths` из `reverse_index` | Новое |

Параллельно — Канал A работает на каждом обычном запросе: каждый ответ daemon'а несёт `_meta.dependent_files`, cache-ci заполняет `reverse_index` при cache-fill.

**Порядок шага 3 критичен.** Invalidate шлётся после commit SQLite-транзакции, не до. Иначе окно: cache-ci принял miss-запрос → форварднул в daemon → daemon ещё не дописал новые данные → клиент получает либо старый payload, либо ошибку. Покрывается отдельным integration-тестом «concurrent invalidate + read».

**Параметры дебаунса/батча оставляем как сейчас:**

- `debounce_ms = 1500` — окно молчания после последнего FS-события для слипания связанных изменений (format-on-save IDE, atomic-rename через tmp+rename, серии modify подряд при autosave).
- `batch_ms = 2000` — максимальное окно сборки batch'а перед принудительной обработкой.

Уменьшение до 0.5-1.0s обсуждалось — оставлено для возможной подкрутки **после реальной эксплуатации**. По умолчанию миграция значения не меняет.

---

## 9. Federation: решение и готовый механизм перехода

Когда daemon на удалённой ноде переиндексирует файл своего репо — invalidate-событие к моему **локальному** cache-ci не приходит. Локальный cache-ci при этом мог закешировать ответ от federated репо (через `/federate/<tool>` forwarding). Возникает stale-окно.

Рассмотренные варианты:

- **a)** Не делать ничего специально для federation. Для federated репо работает TTL fallback + `cache-monitor` по `git pull`/DBConfigUpdate грубо чистит prefix (карточка #355). Hit rate на federated tools сохраняется высоким.
- **b)** Remote daemon шлёт invalidate на свой локальный cache-ci (на той же удалённой ноде). Бесполезно для нашей топологии — кэш живёт на нашей стороне.
- **c)** Backward federation channel — локальный cache-ci подписывается на события удалённой ноды через SSE/long-polling. Дорого.
- **d)** Полностью отключить кэш для federated репо: per-scope флаг `cacheable=false`, все запросы идут direct через federation forward. Stale-reads невозможны, но hit rate на federated tools падает до 0% (а это самый горячий сегмент трафика при работе с 1С).

**Принятое решение: a сейчас, готовность к d по флагу.**

Поведение по умолчанию — a (federated кешируется, cache-monitor + TTL покрывают редкие изменения). Обоснование: в текущей топологии нет групповых пользователей (LibreChat production на 192.0.2.50 не ходит к cache-ci на 192.0.2.10; кэш использует один разработчик). Stale-reads через cache-monitor → ~30 сек окно при git pull, что приемлемо для одного пользователя. Включать d сразу — терять hit rate на самом активном сегменте без реального оправдания.

**В коде закладывается механизм d** — per-scope флаг `cacheable=false` в `config/cache_policy_ci.toml` (раздел `[scopes.<alias>]`). Default — `cacheable=true` (поведение как сейчас). Переключение в режим d делается одним правлением конфига и рестартом cache-ci, без пересборки и без касания кода. Когда групповой сценарий станет реальным (несколько пользователей через будущий LibreChat-канал, например) — флипаем флаг для federated алиасов `ut`/`bp-ss`/`bp-tdk`/`zup`, и stale-reads исключаются.

К c (backward federation channel) можно вернуться позже, если по `/metrics` baseline (см. раздел 6) обнаружится значимый stale-impact на federated tools — что маловероятно при текущей нагрузке.

---

## 10. Ответы на контрольные вопросы из ТЗ (часть 7)

- **Падение indexer'а во время FS-события при инвалидированных entries.** При выбранной последовательности (commit SQLite до invalidate) такого окна нет: либо invalidate ещё не ушёл, и кеш содержит старое (отдаст клиенту, что не лучше но не хуже текущего поведения), либо SQLite уже зафиксирован и invalidate ушёл; перезапуск daemon вернёт состояние из SQLite (source of truth).
- **Потеря 50% FS events под нагрузкой.** TTL fallback (600s/3600s по tool) ловит. Acceptable.
- **Максимальный размер in-memory map при 100K файлов.** Не делаем in-memory hash-map в cache-ci. Reverse_index — другая структура: `DashMap<file_path, HashSet<cache_key>>`. Размер зависит от числа закешированных entries и среднего fan-out на файл, не от общего числа файлов.
- **Симлинк vs реальный файл.** `notify`/`ReadDirectoryChangesW` отслеживают реальные inode-изменения. Edge case (изменение символа симлинка без изменения target) — known limitation, не покрывается этой миграцией.
- **Race FS event vs concurrent lookup на этот же файл.** Канал B атомарен на уровне `DashMap::retain` внутри cache-ci. Конкурентный `get` до invalidate вернёт старый payload, после — miss. Окно — миллисекунды. Acceptable; критический инвариант поддерживается интеграционным тестом из открытого вопроса №2 (см. реализационный roadmap).

---

## 11. Out of scope этой итерации

Целиком — Часть 5 ТЗ: PR, ARCHITECTURE.md, integration tests, load test, migration guide, feature flag. Только аудит-отчёт. Реализация — следующей итерацией после прочтения и согласования.

## Следующие шаги

1. Согласовать раздел 5 (принятое решение) и раздел 9 (federation = MVP a).
2. Реализовать pre-requisite раздела 6: `/metrics` endpoint в cache-ci. Снять hit rate baseline за неделю эксплуатации.
3. Открыть отдельную итерацию с PR-планом по двум репозиториям (`code-index` для шагов 3 и канала A, `mcp-services-cash` для шага 4 и расширения `/invalidate` параметром `file_paths`).
