//! Memory, latency and throughput measurement shared by every scenario and
//! architecture candidate. `process_rss_kib` reuses the exact platform APIs
//! `src/bin/full_runtime_benchmark.rs` and `src/bin/socks5_benchmark.rs`
//! already use — defined once here since (unlike those two standalone
//! bins) this module is shared by both `src/bin/lua_runtime_benchmark.rs`
//! and `tests/lua_runtime.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// This process's own working-set/RSS right now, in KiB. `None` on a
/// platform this benchmark doesn't support reading it on (see "Support at
/// least: Linux, Windows").
#[cfg(target_os = "linux")]
pub fn process_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS")?.trim_start().strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[cfg(windows)]
pub fn process_rss_kib() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb);
        if ok != 0 {
            Some((counters.WorkingSetSize as u64) / 1024)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn process_rss_kib() -> Option<u64> {
    None
}

/// A bounded sample of latencies with a small local quantile helper — no
/// heavy statistics dependency, per the mission's explicit instruction. A
/// full sort on read is fine at this benchmark's sample sizes (at most a
/// few hundred thousand `Duration`s, read once per report).
#[derive(Debug, Default)]
pub struct LatencySamples {
    samples: Vec<Duration>,
    cap: usize,
    dropped_over_cap: u64,
}

impl LatencySamples {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            samples: Vec::with_capacity(cap.min(1 << 20)),
            cap,
            dropped_over_cap: 0,
        }
    }

    pub fn record(&mut self, duration: Duration) {
        if self.samples.len() < self.cap {
            self.samples.push(duration);
        } else {
            self.dropped_over_cap += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn summary(&self) -> LatencySummary {
        if self.samples.is_empty() {
            return LatencySummary::default();
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let quantile = |q: f64| -> Duration {
            let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };
        let sum: Duration = sorted.iter().sum();
        LatencySummary {
            count: sorted.len(),
            min: sorted[0],
            max: sorted[sorted.len() - 1],
            mean: sum / sorted.len() as u32,
            p50: quantile(0.50),
            p95: quantile(0.95),
            p99: quantile(0.99),
            samples_dropped_over_cap: self.dropped_over_cap,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LatencySummary {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub samples_dropped_over_cap: u64,
}

/// Lock-free counters cheap enough to bump from any worker/dispatcher
/// without contention dominating the measurement itself.
#[derive(Debug, Default)]
pub struct ThroughputCounters {
    pub events_dispatched: AtomicU64,
    pub handler_executions: AtomicU64,
    pub commands_emitted: AtomicU64,
    pub handler_errors: AtomicU64,
}

impl ThroughputCounters {
    pub fn snapshot(&self) -> ThroughputSnapshot {
        ThroughputSnapshot {
            events_dispatched: self.events_dispatched.load(Ordering::Relaxed),
            handler_executions: self.handler_executions.load(Ordering::Relaxed),
            commands_emitted: self.commands_emitted.load(Ordering::Relaxed),
            handler_errors: self.handler_errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ThroughputSnapshot {
    pub events_dispatched: u64,
    pub handler_executions: u64,
    pub commands_emitted: u64,
    pub handler_errors: u64,
}

impl ThroughputSnapshot {
    pub fn per_second(&self, elapsed: Duration) -> ThroughputRates {
        let secs = elapsed.as_secs_f64().max(1e-9);
        ThroughputRates {
            events_per_sec: self.events_dispatched as f64 / secs,
            handlers_per_sec: self.handler_executions as f64 / secs,
            commands_per_sec: self.commands_emitted as f64 / secs,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ThroughputRates {
    pub events_per_sec: f64,
    pub handlers_per_sec: f64,
    pub commands_per_sec: f64,
}

/// Every metric a worker (or the Rust-baseline path) writes into, bundled
/// so callers pass one handle instead of four separate `Arc`s — shared by
/// `worker.rs` and `dispatch.rs`, defined here so neither has to depend on
/// the other for it.
pub struct DispatcherMetrics {
    pub throughput: Arc<ThroughputCounters>,
    pub enqueue_to_start: Arc<Mutex<LatencySamples>>,
    pub enqueue_to_complete: Arc<Mutex<LatencySamples>>,
    pub command_latency: Arc<Mutex<LatencySamples>>,
}

impl DispatcherMetrics {
    pub fn new(sample_capacity: usize) -> Self {
        Self {
            throughput: Arc::new(ThroughputCounters::default()),
            enqueue_to_start: Arc::new(Mutex::new(LatencySamples::with_capacity(sample_capacity))),
            enqueue_to_complete: Arc::new(Mutex::new(LatencySamples::with_capacity(
                sample_capacity,
            ))),
            command_latency: Arc::new(Mutex::new(LatencySamples::with_capacity(sample_capacity))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_are_computed_from_a_known_distribution() {
        let mut samples = LatencySamples::with_capacity(100);
        for ms in 1..=100u64 {
            samples.record(Duration::from_millis(ms));
        }
        let summary = samples.summary();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.min, Duration::from_millis(1));
        assert_eq!(summary.max, Duration::from_millis(100));
        // Nearest-rank quantile over a 0-indexed sorted 1..=100ms sample:
        // idx(q) = round((n-1)*q), so p50 lands on the 51st value, not the
        // 50th — verified against the implementation, not assumed.
        assert_eq!(summary.p50, Duration::from_millis(51));
        assert_eq!(summary.p95, Duration::from_millis(95));
        assert_eq!(summary.p99, Duration::from_millis(99));
    }

    #[test]
    fn samples_beyond_capacity_are_counted_not_silently_lost() {
        let mut samples = LatencySamples::with_capacity(2);
        samples.record(Duration::from_millis(1));
        samples.record(Duration::from_millis(2));
        samples.record(Duration::from_millis(3));
        assert_eq!(samples.len(), 2);
        assert_eq!(samples.summary().samples_dropped_over_cap, 1);
    }

    #[test]
    fn throughput_counters_are_consistent_under_concurrent_increments() {
        use std::sync::Arc;
        let counters = Arc::new(ThroughputCounters::default());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = counters.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    c.events_dispatched.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counters.snapshot().events_dispatched, 8000);
    }

    #[test]
    fn process_rss_kib_returns_a_plausible_value_on_supported_platforms() {
        if let Some(kib) = process_rss_kib() {
            assert!(kib > 0, "a running process should report nonzero RSS");
        }
    }
}
