//! Single-flight: при N одновременных одинаковых запросах ровно один идёт
//! в бэкенд, остальные ждут результат через `tokio::sync::broadcast` и
//! получают копию.
//!
//! API:
//! - [`SingleFlight::do_or_join`] — точка входа. Если по ключу уже идёт
//!   запрос — подписывается на его результат; иначе сам выполняет работу
//!   и публикует результат всем подписчикам.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::broadcast;

/// Результат запроса, который можно тиражировать подписчикам без cloning'а
/// payload'а — `Arc<T>`.
pub type Shared<T> = Arc<T>;

/// Координатор single-flight.
#[derive(Debug)]
pub struct SingleFlight<T> {
    inflight: DashMap<String, broadcast::Sender<Result<Shared<T>, String>>>,
}

impl<T> Default for SingleFlight<T> {
    fn default() -> Self {
        Self {
            inflight: DashMap::new(),
        }
    }
}

impl<T> SingleFlight<T>
where
    T: Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Выполнить `work` либо подключиться к уже выполняющемуся запросу с тем же ключом.
    ///
    /// Возвращает `Ok(Shared<T>)` при успехе, `Err(String)` если первая попытка
    /// провалилась — текст ошибки одинаков для всех подписчиков.
    ///
    /// Важно: ключ удаляется из `inflight` ВСЕГДА (через scope-guard через Drop),
    /// даже если работа упадёт с паникой.
    pub async fn do_or_join<F, Fut>(&self, key: String, work: F) -> Result<Shared<T>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        // Попытка стать «лидером».
        let leader_sender = {
            // Если кто-то уже работает по этому ключу — подписываемся.
            if let Some(existing) = self.inflight.get(&key) {
                let mut rx = existing.subscribe();
                // Дроп read-guard'а перед await, иначе блокируем DashMap-shard.
                drop(existing);
                return rx
                    .recv()
                    .await
                    .map_err(|e| format!("singleflight: канал лидера упал: {e}"))?;
            }

            // Никого нет — становимся лидером.
            let (tx, _) = broadcast::channel::<Result<Shared<T>, String>>(16);
            // entry-API на случай гонки на старте.
            let entry = self.inflight.entry(key.clone()).or_insert(tx);
            entry.clone()
        };

        // Гарантированное удаление ключа из inflight по завершению или панике.
        let guard = InflightGuard {
            map: &self.inflight,
            key: &key,
        };

        // Выполняем работу.
        let result: Result<Shared<T>, String> = match work().await {
            Ok(value) => Ok(Arc::new(value)),
            Err(err) => Err(err),
        };

        // Шлём результат всем ожидающим. Если подписчиков нет — ошибка send'а
        // не критична, нас это не отменяет.
        let _ = leader_sender.send(result.clone());

        // Явное освобождение перед возвратом.
        drop(guard);
        result
    }
}

/// RAII-guard: убирает ключ из inflight даже если future с work() панично выходит.
struct InflightGuard<'a, T> {
    map: &'a DashMap<String, broadcast::Sender<Result<Shared<T>, String>>>,
    key: &'a str,
}

impl<'a, T> Drop for InflightGuard<'a, T> {
    fn drop(&mut self) {
        self.map.remove(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn parallel_calls_collapse_to_one_backend_hit() {
        let sf: Arc<SingleFlight<String>> = Arc::new(SingleFlight::new());
        let backend_calls = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..10 {
            let sf = sf.clone();
            let counter = backend_calls.clone();
            handles.push(tokio::spawn(async move {
                sf.do_or_join("key".into(), || async move {
                    // Имитация похода в бэкенд.
                    counter.fetch_add(1, Ordering::SeqCst);
                    sleep(Duration::from_millis(50)).await;
                    Ok::<_, String>("response".to_string())
                })
                .await
            }));
        }

        let results: Vec<_> = futures_util::future::join_all(handles).await;

        // Все 10 получили валидный результат.
        for r in &results {
            let inner = r.as_ref().unwrap();
            assert!(inner.is_ok());
            assert_eq!(**inner.as_ref().unwrap(), "response".to_string());
        }
        // Но в бэкенд сходили только один раз.
        assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn error_is_broadcast_to_all_followers() {
        let sf: Arc<SingleFlight<String>> = Arc::new(SingleFlight::new());

        let mut handles = vec![];
        for _ in 0..3 {
            let sf = sf.clone();
            handles.push(tokio::spawn(async move {
                sf.do_or_join("err_key".into(), || async {
                    sleep(Duration::from_millis(20)).await;
                    Err::<String, _>("backend exploded".to_string())
                })
                .await
            }));
        }

        let results: Vec<_> = futures_util::future::join_all(handles).await;
        for r in results {
            let inner = r.unwrap();
            assert!(inner.is_err());
            assert_eq!(inner.unwrap_err(), "backend exploded");
        }
    }

    #[tokio::test]
    async fn distinct_keys_run_independently() {
        let sf: Arc<SingleFlight<String>> = Arc::new(SingleFlight::new());
        let backend_calls = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for k in ["a", "b", "c"] {
            let sf = sf.clone();
            let counter = backend_calls.clone();
            handles.push(tokio::spawn(async move {
                sf.do_or_join(k.into(), || async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, String>(k.to_string())
                })
                .await
            }));
        }

        let _ = futures_util::future::join_all(handles).await;
        assert_eq!(backend_calls.load(Ordering::SeqCst), 3);
    }
}
