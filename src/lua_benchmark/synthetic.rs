//! The synthetic event-dispatch benchmark: no real Minecraft connection,
//! just deterministic event generation against every architecture
//! candidate — the primary way this benchmark answers "how much does Lua
//! add" and "at what event rate does a VM saturate" without full-runtime
//! network overhead confounding the measurement (see
//! `full_runtime.rs` for the real-connection counterpart).

use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::command::CommandSink;
use super::dispatch::{Architecture, Dispatcher, QueueKind};
use super::event::{BenchEvent, BotId, Envelope, SizeClass};
use super::metrics::{process_rss_kib, DispatcherMetrics, LatencySummary, ThroughputSnapshot};
use super::sandbox::SandboxConfig;

/// The five event-rate scenarios the mission specifies, plus the explicit
/// 800x20/s upper-pressure case as a named `Pathological` variant rather
/// than a hidden magic number — see "800 bots x 20 callbacks/s" in
/// `docs/lua_runtime_benchmark.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EventRateProfile {
    /// One meaningful event every 5 seconds per bot.
    Low,
    /// One event per second per bot.
    Typical,
    /// Five events per second per bot.
    Busy,
    /// Every bot emits once within a short window, then idles — models a
    /// reconnect storm or a world event landing on the whole swarm at once.
    Burst,
    /// A deliberately excessive fixed rate, used only to find the
    /// saturation point — never the intended steady-state API.
    Pathological(f64),
}

impl EventRateProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "typical" => Some(Self::Typical),
            "busy" => Some(Self::Busy),
            "burst" => Some(Self::Burst),
            "pathological" => Some(Self::Pathological(20.0)),
            other => other
                .strip_prefix("pathological:")
                .and_then(|rate| rate.parse().ok())
                .map(Self::Pathological),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Low => "low".to_string(),
            Self::Typical => "typical".to_string(),
            Self::Busy => "busy".to_string(),
            Self::Burst => "burst".to_string(),
            Self::Pathological(rate) => format!("pathological({rate}/s)"),
        }
    }

    fn events_per_sec_per_bot(self) -> f64 {
        match self {
            Self::Low => 0.2,
            Self::Typical => 1.0,
            Self::Busy => 5.0,
            Self::Burst => 0.0, // handled separately: one shot, not steady-state
            Self::Pathological(rate) => rate,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyntheticConfig {
    pub bot_count: u32,
    pub rate: EventRateProfile,
    pub duration: Duration,
    pub seed: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RssStages {
    pub baseline: Option<u64>,
    pub after_dispatcher_start: Option<u64>,
    pub after_events: Option<u64>,
    pub after_shutdown: Option<u64>,
    pub after_cleanup_wait: Option<u64>,
}

pub struct SyntheticResult {
    pub architecture_label: String,
    pub script_label: &'static str,
    pub rate_label: String,
    pub bot_count: u32,
    pub wall_time: Duration,
    pub rss: RssStages,
    pub throughput: ThroughputSnapshot,
    pub throughput_rates: super::metrics::ThroughputRates,
    pub enqueue_to_start: LatencySummary,
    pub enqueue_to_complete: LatencySummary,
    pub command_latency: LatencySummary,
    pub command_sink_emitted: u64,
    pub command_sink_dropped: u64,
    pub queue_peak_depth_total: usize,
    pub queue_dropped_total: u64,
    pub worker_reports: Vec<Result<super::worker::WorkerReport, String>>,
}

/// The step size the per-bot event schedulers advance by — fine enough to
/// pace `Busy` (5/s) and low `Pathological` rates accurately, coarse enough
/// not to burn the driver thread's own CPU dominating the measurement.
const SCHEDULE_STEP: Duration = Duration::from_millis(10);

/// Runs one full synthetic scenario: starts the dispatcher, paces
/// deterministic event generation for `config.duration` at `config.rate`,
/// shuts the dispatcher down, and reports every stage's RSS plus the full
/// metrics set. `queue_capacity` sizes both the FIFO and priority queues'
/// bounded lanes identically so architecture comparisons aren't skewed by
/// incidental capacity differences.
#[allow(clippy::too_many_arguments)]
pub fn run_synthetic_scenario(
    config: SyntheticConfig,
    architecture: Architecture,
    queue_kind: QueueKind,
    sandbox_config: SandboxConfig,
    script_body: &'static str,
    script_label: &'static str,
    command_sink_capacity: usize,
    cleanup_wait: Duration,
) -> SyntheticResult {
    let wall_start = Instant::now();
    let mut rss = RssStages {
        baseline: process_rss_kib(),
        ..Default::default()
    };

    let (sink, command_rx) = CommandSink::bounded(command_sink_capacity);
    let sink_stats_handle = sink.clone();
    let metrics = std::sync::Arc::new(DispatcherMetrics::new(1 << 20));
    let dispatcher = Dispatcher::start(
        architecture,
        queue_kind,
        sandbox_config,
        script_body,
        sink,
        metrics.clone(),
    );
    rss.after_dispatcher_start = process_rss_kib();

    generate_events(&dispatcher, config);
    rss.after_events = process_rss_kib();

    let queue_peak_depth_total = dispatcher.queue_peak_depth_total();
    let queue_dropped_total = dispatcher.queue_dropped_total();
    let worker_reports = dispatcher.shutdown();
    rss.after_shutdown = process_rss_kib();

    // Drain whatever the command sink still holds so its receiver drops
    // cleanly, then give the allocator a moment before the final reading —
    // matches "allow bounded cleanup time" rather than sampling instantly.
    while command_rx.try_recv().is_ok() {}
    drop(command_rx);
    std::thread::sleep(cleanup_wait);
    rss.after_cleanup_wait = process_rss_kib();

    let throughput = metrics.throughput.snapshot();
    let wall_time = wall_start.elapsed();
    let sink_stats = sink_stats_handle.stats();
    let enqueue_to_start = metrics.enqueue_to_start.lock().unwrap().summary();
    let enqueue_to_complete = metrics.enqueue_to_complete.lock().unwrap().summary();
    let command_latency = metrics.command_latency.lock().unwrap().summary();

    SyntheticResult {
        architecture_label: architecture.label(),
        script_label,
        rate_label: config.rate.label(),
        bot_count: config.bot_count,
        wall_time,
        rss,
        throughput,
        throughput_rates: throughput.per_second(config.duration),
        enqueue_to_start,
        enqueue_to_complete,
        command_latency,
        command_sink_emitted: sink_stats.emitted,
        command_sink_dropped: sink_stats.dropped,
        queue_peak_depth_total,
        queue_dropped_total,
        worker_reports,
    }
}

/// Deterministic per-bot event scheduling: each bot has its own fractional
/// accumulator advanced by `rate * step` every step; firing when it crosses
/// 1.0 keeps every bot's long-run average exactly at the configured rate
/// without needing per-event randomness for *timing* (only for which size
/// class an event is, which stays seeded and reproducible).
fn generate_events(dispatcher: &Dispatcher, config: SyntheticConfig) {
    let mut rng = StdRng::seed_from_u64(config.seed);

    if matches!(config.rate, EventRateProfile::Burst) {
        for bot in 0..config.bot_count {
            let size = weighted_size_class(&mut rng);
            dispatcher.dispatch(Envelope {
                bot_id: BotId(bot),
                event: BenchEvent::synthetic(size, BotId(bot), 0),
                enqueued_at: Instant::now(),
                bot_seq: 0,
            });
        }
        return;
    }

    let per_step = config.rate.events_per_sec_per_bot() * SCHEDULE_STEP.as_secs_f64();
    let mut accumulators = vec![0f64; config.bot_count as usize];
    let mut seqs = vec![0u64; config.bot_count as usize];
    let deadline = Instant::now() + config.duration;

    while Instant::now() < deadline {
        let step_start = Instant::now();
        for bot in 0..config.bot_count {
            let acc = &mut accumulators[bot as usize];
            *acc += per_step;
            if *acc >= 1.0 {
                *acc -= 1.0;
                let size = weighted_size_class(&mut rng);
                let seq = &mut seqs[bot as usize];
                dispatcher.dispatch(Envelope {
                    bot_id: BotId(bot),
                    event: BenchEvent::synthetic(size, BotId(bot), *seq),
                    enqueued_at: Instant::now(),
                    bot_seq: *seq,
                });
                *seq += 1;
            }
        }
        let elapsed = step_start.elapsed();
        if elapsed < SCHEDULE_STEP {
            std::thread::sleep(SCHEDULE_STEP - elapsed);
        }
    }
}

/// 70% small / 25% medium / 5% large — realistic traffic shape (frequent
/// vitals/lifecycle noise, occasional chat/inventory, rare GUI opens), not
/// a uniform mix, matching "include several event payload sizes" without
/// pretending they're equally common in real play.
fn weighted_size_class(rng: &mut StdRng) -> SizeClass {
    let roll: f64 = rng.r#gen();
    if roll < 0.70 {
        SizeClass::Small
    } else if roll < 0.95 {
        SizeClass::Medium
    } else {
        SizeClass::Large
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::scripts::ScriptKind;

    #[test]
    fn rust_baseline_synthetic_scenario_completes_and_reports_metrics() {
        let config = SyntheticConfig {
            bot_count: 5,
            rate: EventRateProfile::Busy,
            duration: Duration::from_millis(200),
            seed: 42,
        };
        let result = run_synthetic_scenario(
            config,
            Architecture::RustBaseline,
            QueueKind::Fifo { capacity: 256 },
            SandboxConfig::default(),
            ScriptKind::Realistic.source(),
            "realistic",
            256,
            Duration::from_millis(10),
        );
        assert!(result.throughput.events_dispatched > 0);
        assert_eq!(result.worker_reports.len(), 0);
    }

    #[test]
    fn shared_vm_synthetic_scenario_completes_and_all_events_are_accounted_for() {
        let config = SyntheticConfig {
            bot_count: 4,
            rate: EventRateProfile::Typical,
            duration: Duration::from_millis(300),
            seed: 7,
        };
        let result = run_synthetic_scenario(
            config,
            Architecture::Lua { worker_count: 1 },
            QueueKind::Fifo { capacity: 1024 },
            SandboxConfig::default(),
            ScriptKind::LightState.source(),
            "light-state",
            256,
            Duration::from_millis(10),
        );
        assert_eq!(result.worker_reports.len(), 1);
        let report = result.worker_reports[0].as_ref().unwrap();
        assert_eq!(
            report.events_processed, result.throughput.events_dispatched,
            "every dispatched event must reach the single worker"
        );
    }

    #[test]
    fn deterministic_seed_produces_the_same_event_count_across_runs() {
        let config = SyntheticConfig {
            bot_count: 3,
            rate: EventRateProfile::Typical,
            duration: Duration::from_millis(150),
            seed: 99,
        };
        let run = |cfg: SyntheticConfig| {
            run_synthetic_scenario(
                cfg,
                Architecture::RustBaseline,
                QueueKind::Fifo { capacity: 64 },
                SandboxConfig::default(),
                ScriptKind::NoOp.source(),
                "no-op",
                64,
                Duration::from_millis(5),
            )
            .throughput
            .events_dispatched
        };
        let a = run(config);
        let b = run(config);
        assert_eq!(a, b, "same seed/config must schedule the same event count");
    }

    #[test]
    fn burst_profile_fires_exactly_one_event_per_bot() {
        let config = SyntheticConfig {
            bot_count: 10,
            rate: EventRateProfile::Burst,
            duration: Duration::from_millis(50),
            seed: 1,
        };
        let result = run_synthetic_scenario(
            config,
            Architecture::RustBaseline,
            QueueKind::Fifo { capacity: 64 },
            SandboxConfig::default(),
            ScriptKind::NoOp.source(),
            "no-op",
            64,
            Duration::from_millis(5),
        );
        assert_eq!(result.throughput.events_dispatched, 10);
    }
}
