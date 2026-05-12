//! Счётчики hit/miss и среднего latency бэкенда.
//!
//! Все операции — atomic, без блокировок. Среднее latency считается как
//! экспоненциальное скользящее среднее (EMA) с коэффициентом 0.1: достаточно
//! чтобы сгладить выбросы и не возиться с гистограммой.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Метрики одного экземпляра прокси.
#[derive(Debug, Default)]
pub struct Metrics {
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub bypass: AtomicU64,
    pub backend_errors: AtomicU64,
    pub cache_size: AtomicUsize,
    /// Размер реверс-индекса `file_path → cache_keys`. Обновляется handler'ом
    /// `/metrics` через `update_reverse_index_size(cache.reverse_index_size())`
    /// — отдельной фоновой синхронизации не требуется, цифра «живая на момент
    /// чтения».
    pub reverse_index_size: AtomicUsize,
    /// EMA latency бэкенда в микросекундах. Хранится как `u64`-битовое
    /// представление `f64` для атомарного обмена.
    avg_backend_latency_micros_bits: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bypass(&self) {
        self.bypass.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_backend_error(&self) {
        self.backend_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn update_cache_size(&self, len: usize) {
        self.cache_size.store(len, Ordering::Relaxed);
    }

    pub fn update_reverse_index_size(&self, len: usize) {
        self.reverse_index_size.store(len, Ordering::Relaxed);
    }

    /// Записать новое замерение latency. Внутри — EMA с коэффициентом 0.1.
    pub fn observe_backend_latency_micros(&self, micros: u64) {
        const ALPHA: f64 = 0.1;
        loop {
            let prev_bits = self.avg_backend_latency_micros_bits.load(Ordering::Relaxed);
            let prev = f64::from_bits(prev_bits);
            let new = if prev_bits == 0 {
                micros as f64
            } else {
                ALPHA * (micros as f64) + (1.0 - ALPHA) * prev
            };
            let new_bits = new.to_bits();
            // CAS на тот случай, если параллельный поток уже обновил значение.
            if self
                .avg_backend_latency_micros_bits
                .compare_exchange(prev_bits, new_bits, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let avg_bits = self.avg_backend_latency_micros_bits.load(Ordering::Relaxed);
        let avg_micros = if avg_bits == 0 {
            0.0
        } else {
            f64::from_bits(avg_bits)
        };
        MetricsSnapshot {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            bypass: self.bypass.load(Ordering::Relaxed),
            backend_errors: self.backend_errors.load(Ordering::Relaxed),
            cache_size: self.cache_size.load(Ordering::Relaxed),
            reverse_index_size: self.reverse_index_size.load(Ordering::Relaxed),
            avg_backend_latency_ms: avg_micros / 1000.0,
        }
    }

    /// Сериализация в Prometheus text exposition format (v0.0.4).
    /// Для отдачи через `GET /metrics` — это де-факто стандарт для всех
    /// systems-мониторингов (Prometheus, Grafana Agent, VictoriaMetrics,
    /// node_exporter и т.п.).
    ///
    /// `server_alias` (`"ci"` / `"1c"`) попадает в label `server`, чтобы
    /// при сборе с обоих прокси одним job'ом метрики не схлопывались.
    pub fn to_prometheus_text(&self, server_alias: &str) -> String {
        let snap = self.snapshot();
        let mut out = String::with_capacity(1024);
        let _ = writeln!(out, "# HELP cache_hits_total Total number of cache hits.");
        let _ = writeln!(out, "# TYPE cache_hits_total counter");
        let _ = writeln!(
            out,
            "cache_hits_total{{server=\"{server_alias}\"}} {}",
            snap.cache_hits
        );
        let _ = writeln!(
            out,
            "# HELP cache_misses_total Total number of cache misses (backend was called)."
        );
        let _ = writeln!(out, "# TYPE cache_misses_total counter");
        let _ = writeln!(
            out,
            "cache_misses_total{{server=\"{server_alias}\"}} {}",
            snap.cache_misses
        );
        let _ = writeln!(
            out,
            "# HELP cache_bypass_total Total number of explicit cache bypasses (non-cacheable tool, scope disabled, X-Cache-Bypass header)."
        );
        let _ = writeln!(out, "# TYPE cache_bypass_total counter");
        let _ = writeln!(
            out,
            "cache_bypass_total{{server=\"{server_alias}\"}} {}",
            snap.bypass
        );
        let _ = writeln!(
            out,
            "# HELP cache_backend_errors_total Total number of failed backend calls."
        );
        let _ = writeln!(out, "# TYPE cache_backend_errors_total counter");
        let _ = writeln!(
            out,
            "cache_backend_errors_total{{server=\"{server_alias}\"}} {}",
            snap.backend_errors
        );
        let _ = writeln!(
            out,
            "# HELP cache_entries_count Current number of entries in the main cache."
        );
        let _ = writeln!(out, "# TYPE cache_entries_count gauge");
        let _ = writeln!(
            out,
            "cache_entries_count{{server=\"{server_alias}\"}} {}",
            snap.cache_size
        );
        let _ = writeln!(
            out,
            "# HELP cache_reverse_index_size Number of file_paths registered in reverse_index for point invalidation."
        );
        let _ = writeln!(out, "# TYPE cache_reverse_index_size gauge");
        let _ = writeln!(
            out,
            "cache_reverse_index_size{{server=\"{server_alias}\"}} {}",
            snap.reverse_index_size
        );
        let _ = writeln!(
            out,
            "# HELP cache_backend_latency_ms_avg EMA of backend call latency in milliseconds."
        );
        let _ = writeln!(out, "# TYPE cache_backend_latency_ms_avg gauge");
        let _ = writeln!(
            out,
            "cache_backend_latency_ms_avg{{server=\"{server_alias}\"}} {}",
            snap.avg_backend_latency_ms
        );
        out
    }
}

/// Снимок метрик в человекочитаемом виде. Сериализуется в JSON для `/metrics/json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSnapshot {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub bypass: u64,
    pub backend_errors: u64,
    pub cache_size: usize,
    pub reverse_index_size: usize,
    pub avg_backend_latency_ms: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_first_observation_equals_value() {
        let m = Metrics::new();
        m.observe_backend_latency_micros(10_000);
        let snap = m.snapshot();
        assert!((snap.avg_backend_latency_ms - 10.0).abs() < 1e-9);
    }

    #[test]
    fn ema_smoothes_outliers() {
        let m = Metrics::new();
        for _ in 0..100 {
            m.observe_backend_latency_micros(10_000);
        }
        m.observe_backend_latency_micros(1_000_000);
        let snap = m.snapshot();
        // 90% от 10мс, 10% от 1000мс ≈ 109мс. Главное — выброс не уехал в 1000.
        assert!(snap.avg_backend_latency_ms < 200.0);
    }

    #[test]
    fn prometheus_text_contains_all_metrics() {
        let m = Metrics::new();
        m.record_hit();
        m.record_hit();
        m.record_miss();
        m.record_bypass();
        m.record_backend_error();
        m.update_cache_size(7);
        m.update_reverse_index_size(3);
        m.observe_backend_latency_micros(42_000);

        let text = m.to_prometheus_text("ci");
        assert!(text.contains("cache_hits_total{server=\"ci\"} 2"));
        assert!(text.contains("cache_misses_total{server=\"ci\"} 1"));
        assert!(text.contains("cache_bypass_total{server=\"ci\"} 1"));
        assert!(text.contains("cache_backend_errors_total{server=\"ci\"} 1"));
        assert!(text.contains("cache_entries_count{server=\"ci\"} 7"));
        assert!(text.contains("cache_reverse_index_size{server=\"ci\"} 3"));
        assert!(text.contains("cache_backend_latency_ms_avg{server=\"ci\"}"));
        assert!(text.contains("# HELP cache_hits_total"));
        assert!(text.contains("# TYPE cache_hits_total counter"));
    }

    #[test]
    fn prometheus_text_uses_provided_server_label() {
        let m = Metrics::new();
        let text = m.to_prometheus_text("1c");
        assert!(text.contains("cache_hits_total{server=\"1c\"}"));
        assert!(!text.contains("cache_hits_total{server=\"ci\"}"));
    }
}
