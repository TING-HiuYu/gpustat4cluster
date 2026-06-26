use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use common::{ErrorCode, ServerGresSnapshot};

use crate::collector::GresCollector;

const LATENCY_SAMPLE_LIMIT: usize = 128;

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub snapshot: Arc<ServerGresSnapshot>,
    collected_at: Instant,
}

impl CacheEntry {
    fn is_expired(&self, ttl_ms: u64, now: Instant) -> bool {
        now.duration_since(self.collected_at).as_millis() as u64 >= ttl_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheMetricsSnapshot {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub merge_count: u64,
    pub collect_count: u64,
    pub avg_collect_latency_us: u64,
    pub collect_latency_p50_us: u64,
    pub collect_latency_p95_us: u64,
    pub cache_hit_rate_bps: u64,
    pub cache_miss_rate_bps: u64,
    pub merge_ratio_bps: u64,
}

#[derive(Debug, Default)]
pub struct CacheMetrics {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    merge_count: AtomicU64,
    collect_count: AtomicU64,
    collect_latency_total_ns: AtomicU64,
    collect_latency_samples_us: Mutex<VecDeque<u64>>,
}

impl CacheMetrics {
    fn record_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn record_merge(&self) {
        self.merge_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_collect_latency(&self, elapsed: Duration) {
        let elapsed_us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.collect_count.fetch_add(1, Ordering::Relaxed);
        self.collect_latency_total_ns.fetch_add(
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if let Ok(mut samples) = self.collect_latency_samples_us.lock() {
            if samples.len() == LATENCY_SAMPLE_LIMIT {
                samples.pop_front();
            }
            samples.push_back(elapsed_us);
        }
    }

    pub fn snapshot(&self) -> CacheMetricsSnapshot {
        let cache_hits = self.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.cache_misses.load(Ordering::Relaxed);
        let merge_count = self.merge_count.load(Ordering::Relaxed);
        let collect_count = self.collect_count.load(Ordering::Relaxed);
        let total_ns = self.collect_latency_total_ns.load(Ordering::Relaxed);
        let total_requests = cache_hits + cache_misses;
        let latency_samples: Vec<u64> = self
            .collect_latency_samples_us
            .lock()
            .map(|samples| samples.iter().copied().collect())
            .unwrap_or_default();

        CacheMetricsSnapshot {
            cache_hits,
            cache_misses,
            merge_count,
            collect_count,
            avg_collect_latency_us: if collect_count == 0 {
                0
            } else {
                total_ns / collect_count / 1_000
            },
            collect_latency_p50_us: percentile_us(latency_samples.clone(), 50),
            collect_latency_p95_us: percentile_us(latency_samples, 95),
            cache_hit_rate_bps: ratio_bps(cache_hits, total_requests),
            cache_miss_rate_bps: ratio_bps(cache_misses, total_requests),
            merge_ratio_bps: ratio_bps(merge_count, cache_misses),
        }
    }
}

#[derive(Debug, Default)]
struct RefreshState {
    in_flight: bool,
    generation: u64,
    last_error: Option<ErrorCode>,
}

#[derive(Debug)]
pub struct GresCache {
    entry: RwLock<Option<CacheEntry>>,
    refresh_state: Mutex<RefreshState>,
    refreshed: Condvar,
    metrics: CacheMetrics,
}

impl GresCache {
    pub fn new() -> Self {
        Self {
            entry: RwLock::new(None),
            refresh_state: Mutex::new(RefreshState::default()),
            refreshed: Condvar::new(),
            metrics: CacheMetrics::default(),
        }
    }

    pub fn get_or_refresh(
        self: &Arc<Self>,
        collector: &dyn GresCollector,
        ttl_ms: u64,
    ) -> Result<CacheEntry, ErrorCode> {
        if let Some(hit) = self.get_fresh(ttl_ms, Instant::now()) {
            self.metrics.record_hit();
            return Ok(hit);
        }

        self.metrics.record_miss();
        let mut state = self.refresh_state.lock().map_err(|_| ErrorCode::Internal)?;
        let observed_generation = state.generation;

        loop {
            if let Some(hit) = self.get_fresh(ttl_ms, Instant::now()) {
                return Ok(hit);
            }

            if state.generation != observed_generation {
                if let Some(code) = state.last_error {
                    return Err(code);
                }
                return Err(ErrorCode::Internal);
            }

            if !state.in_flight {
                state.in_flight = true;
                state.last_error = None;
                break;
            }

            self.metrics.record_merge();
            state = self
                .refreshed
                .wait(state)
                .map_err(|_| ErrorCode::Internal)?;
        }

        drop(state);
        let started = Instant::now();
        let result = collector.collect_gres_snapshot().and_then(|snapshot| {
            let snapshot = omit_process_commands(snapshot);
            Ok(CacheEntry {
                snapshot: Arc::new(snapshot),
                collected_at: Instant::now(),
            })
        });
        self.metrics.record_collect_latency(started.elapsed());

        match result {
            Ok(entry) => {
                self.store(entry.clone());
                self.finish_refresh(None);
                Ok(entry)
            }
            Err(code) => {
                self.finish_refresh(Some(code));
                Err(code)
            }
        }
    }

    pub fn get_latest_or_refresh(
        self: &Arc<Self>,
        collector: &dyn GresCollector,
        ttl_ms: u64,
    ) -> Result<CacheEntry, ErrorCode> {
        if let Some(entry) = self.latest() {
            self.metrics.record_hit();
            return Ok(entry);
        }

        self.get_or_refresh(collector, ttl_ms)
    }

    pub fn metrics(&self) -> CacheMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn latest(&self) -> Option<CacheEntry> {
        self.entry.read().ok().and_then(|g| (*g).clone())
    }

    fn get_fresh(&self, ttl_ms: u64, now: Instant) -> Option<CacheEntry> {
        self.entry.read().ok().and_then(|g| {
            let entry = (*g).clone()?;
            (!entry.is_expired(ttl_ms, now)).then_some(entry)
        })
    }

    fn store(&self, entry: CacheEntry) {
        if let Ok(mut w) = self.entry.write() {
            *w = Some(entry);
        }
    }

    fn finish_refresh(&self, error: Option<ErrorCode>) {
        if let Ok(mut state) = self.refresh_state.lock() {
            state.in_flight = false;
            state.generation = state.generation.wrapping_add(1);
            state.last_error = error;
            self.refreshed.notify_all();
        }
    }
}

fn omit_process_commands(mut snapshot: ServerGresSnapshot) -> ServerGresSnapshot {
    for gres in &mut snapshot.gres {
        for process in &mut gres.processes {
            process.command = None;
        }
    }
    snapshot
}

fn ratio_bps(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        0
    } else {
        numerator.saturating_mul(10_000) / denominator
    }
}

fn percentile_us(mut samples: Vec<u64>, percentile: u64) -> u64 {
    if samples.is_empty() {
        return 0;
    }

    samples.sort_unstable();
    let rank = ((samples.len() as u64 * percentile).saturating_add(99) / 100)
        .saturating_sub(1)
        .min(samples.len() as u64 - 1);
    samples[rank as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{GresInfo, GresMemory, GresProcessInfo, GresUtilization};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    struct CountingCollector {
        calls: AtomicUsize,
        fail: bool,
        sleep_ms: u64,
    }

    impl CountingCollector {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
                sleep_ms: 0,
            }
        }

        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
                sleep_ms: 0,
            }
        }

        fn failing_slow(sleep_ms: u64) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
                sleep_ms,
            }
        }

        fn slow(sleep_ms: u64) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
                sleep_ms,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl GresCollector for CountingCollector {
        fn collect_gres(&self) -> Result<crate::collector::GresNodeSnapshot, ErrorCode> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.sleep_ms > 0 {
                thread::sleep(Duration::from_millis(self.sleep_ms));
            }
            if self.fail {
                return Err(ErrorCode::NvmlUnavailable);
            }
            Ok(crate::collector::GresNodeSnapshot::from_gres_snapshot(
                ServerGresSnapshot {
                    hostname: "test-host".to_string(),
                    driver_version: None,
                    gres: vec![GresInfo {
                        index: 0,
                        name: "test-gres".to_string(),
                        temperature_c: None,
                        uuid: Some(format!("GRES-{call}")),
                        memory: GresMemory {
                            used_mb: call as u64,
                            total_mb: 80,
                        },
                        utilization: GresUtilization {
                            gres_percent: (call as u8).min(100),
                            memory_percent: 5,
                        },
                        processes: vec![GresProcessInfo {
                            pid: 1234,
                            uid: 1000,
                            command: Some("python train.py".to_string()),
                            used_memory_mb: call as u64,
                        }],
                    }],
                },
            ))
        }
    }

    #[test]
    fn ttl_cache_hit_reuses_entry() {
        let cache = Arc::new(GresCache::new());
        let collector = CountingCollector::new();

        let first = cache.get_or_refresh(&collector, 1_000).unwrap();
        let second = cache.get_or_refresh(&collector, 1_000).unwrap();

        assert_eq!(collector.calls(), 1);
        assert!(Arc::ptr_eq(&first.snapshot, &second.snapshot));
        assert_eq!(
            first.snapshot.gres[0].utilization.gres_percent,
            second.snapshot.gres[0].utilization.gres_percent
        );
        let metrics = cache.metrics();
        assert_eq!(metrics.cache_hits, 1);
        assert_eq!(metrics.cache_misses, 1);
        assert_eq!(metrics.cache_hit_rate_bps, 5_000);
        assert_eq!(metrics.cache_miss_rate_bps, 5_000);
    }

    #[test]
    fn ttl_expiry_refreshes_entry() {
        let cache = Arc::new(GresCache::new());
        let collector = CountingCollector::new();

        let first = cache.get_or_refresh(&collector, 1).unwrap();
        thread::sleep(Duration::from_millis(3));
        let second = cache.get_or_refresh(&collector, 1).unwrap();

        assert_eq!(collector.calls(), 2);
        assert_ne!(
            first.snapshot.gres[0].utilization.gres_percent,
            second.snapshot.gres[0].utilization.gres_percent
        );
        assert_eq!(cache.metrics().cache_misses, 2);
    }

    #[test]
    fn concurrent_stale_requests_are_coalesced() {
        let cache = Arc::new(GresCache::new());
        let collector = Arc::new(CountingCollector::slow(25));
        let barrier = Arc::new(Barrier::new(64));

        let mut handles = Vec::new();
        for _ in 0..64 {
            let cache = Arc::clone(&cache);
            let collector = Arc::clone(&collector);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                cache.get_or_refresh(collector.as_ref(), 1_000).unwrap()
            }));
        }

        let entries: Vec<CacheEntry> = handles
            .into_iter()
            .map(|h| h.join().expect("worker thread"))
            .collect();

        assert_eq!(collector.calls(), 1);
        assert!(entries
            .iter()
            .all(|entry| Arc::ptr_eq(&entry.snapshot, &entries[0].snapshot)));
        let metrics = cache.metrics();
        assert_eq!(metrics.cache_misses, 64);
        assert_eq!(metrics.collect_count, 1);
        assert!(metrics.merge_count >= 1);
        assert!(metrics.merge_ratio_bps > 0);
        assert!(metrics.avg_collect_latency_us > 0);
        assert!(metrics.collect_latency_p50_us > 0);
        assert!(metrics.collect_latency_p95_us >= metrics.collect_latency_p50_us);
    }

    #[test]
    fn concurrent_stale_failures_are_coalesced_without_deadlock() {
        let cache = Arc::new(GresCache::new());
        let collector = Arc::new(CountingCollector::failing_slow(25));
        let barrier = Arc::new(Barrier::new(32));

        let mut handles = Vec::new();
        for _ in 0..32 {
            let cache = Arc::clone(&cache);
            let collector = Arc::clone(&collector);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                cache.get_or_refresh(collector.as_ref(), 1_000).unwrap_err()
            }));
        }

        let errors: Vec<ErrorCode> = handles
            .into_iter()
            .map(|h| h.join().expect("worker thread"))
            .collect();

        assert_eq!(collector.calls(), 1);
        assert!(errors
            .iter()
            .all(|code| *code == ErrorCode::NvmlUnavailable));
        let metrics = cache.metrics();
        assert_eq!(metrics.cache_misses, 32);
        assert_eq!(metrics.collect_count, 1);
        assert!(metrics.merge_count >= 1);
    }

    #[test]
    fn collector_failure_returns_degraded_error() {
        let cache = Arc::new(GresCache::new());
        let collector = CountingCollector::failing();

        let err = cache.get_or_refresh(&collector, 10).unwrap_err();

        assert_eq!(err, ErrorCode::NvmlUnavailable);
        assert_eq!(collector.calls(), 1);
        assert_eq!(cache.metrics().collect_count, 1);
    }

    #[test]
    fn cache_metrics_continue_after_error_then_success() {
        let cache = Arc::new(GresCache::new());
        let failing = CountingCollector::failing();
        let success = CountingCollector::new();

        let err = cache.get_or_refresh(&failing, 1_000).unwrap_err();
        let entry = cache.get_or_refresh(&success, 1_000).unwrap();

        assert_eq!(err, ErrorCode::NvmlUnavailable);
        assert_eq!(entry.snapshot.gres.len(), 1);
        assert_eq!(failing.calls(), 1);
        assert_eq!(success.calls(), 1);

        let metrics = cache.metrics();
        assert_eq!(metrics.cache_misses, 2);
        assert_eq!(metrics.collect_count, 2);
        assert_eq!(metrics.cache_hits, 0);
    }

    #[test]
    fn collect_latency_percentiles_use_recent_samples() {
        assert_eq!(percentile_us(vec![], 50), 0);
        assert_eq!(percentile_us(vec![10, 30, 20], 50), 20);
        assert_eq!(percentile_us(vec![10, 20, 30, 40], 95), 40);
        assert_eq!(ratio_bps(1, 4), 2_500);
    }

    #[test]
    fn cached_snapshot_omits_process_commands() {
        let cache = Arc::new(GresCache::new());
        let collector = CountingCollector::new();

        let entry = cache.get_or_refresh(&collector, 1_000).unwrap();

        assert_eq!(entry.snapshot.hostname, "test-host");
        assert_eq!(entry.snapshot.gres[0].processes[0].command, None);
    }
}
