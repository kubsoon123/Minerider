//! The architecture candidates this benchmark compares, unified behind one
//! [`Dispatcher`].
//!
//! Candidates B (one shared VM), C/D/E (2/4/8 workers) and F (one VM per
//! bot) all turn out to be the *same* mechanism at different worker counts:
//! every worker is a persistent `Lua` on its own dedicated `std::thread`,
//! bots are assigned to `worker_index = bot_id % worker_count`. At
//! `worker_count == bot_count`, that assignment is 1:1 — Candidate F,
//! without a second implementation to keep in sync (and without needing
//! `mlua`'s `send` feature at all, since no `Lua` ever crosses an
//! `.await` — see `worker.rs`). Candidate A (no Lua) is the odd one out by
//! necessity: there is no VM to route to.

use std::sync::Arc;
use std::thread::JoinHandle;

use super::command::{BenchCommand, CommandSink};
use super::event::{BenchEvent, BotId, Envelope};
use super::metrics::DispatcherMetrics;
use super::queue::{FifoQueue, OverflowPolicy, PriorityQueue, QueueDesign};
use super::sandbox::SandboxConfig;
use super::worker::{run_worker, WorkerQueue, WorkerReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    RustBaseline,
    /// `worker_count == 1` is "one shared Lua VM" (Candidate B);
    /// `worker_count == bot_count` is "one Lua VM per bot" (Candidate F,
    /// the control comparison); anything in between is a fixed worker pool
    /// (Candidates C/D/E at 2/4/8).
    Lua {
        worker_count: usize,
    },
}

impl Architecture {
    pub fn parse(mode: &str, worker_count: usize, bot_count: usize) -> Option<Self> {
        match mode {
            "rust-baseline" => Some(Self::RustBaseline),
            "shared-lua" => Some(Self::Lua { worker_count: 1 }),
            "worker-pool" => Some(Self::Lua { worker_count }),
            "per-bot-lua" => Some(Self::Lua {
                worker_count: bot_count.max(1),
            }),
            _ => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::RustBaseline => "rust-baseline".to_string(),
            Self::Lua { worker_count: 1 } => "shared-lua(1)".to_string(),
            Self::Lua { worker_count } => format!("worker-pool({worker_count})"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum QueueKind {
    Fifo {
        capacity: usize,
    },
    Priority {
        high_capacity: usize,
        low_capacity: usize,
    },
}

fn build_queue(kind: QueueKind) -> QueueDesign {
    match kind {
        QueueKind::Fifo { capacity } => {
            QueueDesign::Fifo(FifoQueue::new(capacity, OverflowPolicy::DropOldest))
        }
        QueueKind::Priority {
            high_capacity,
            low_capacity,
        } => QueueDesign::Priority(PriorityQueue::new(high_capacity, low_capacity)),
    }
}

/// A running set of workers (or the Rust-baseline no-Lua path) a scenario
/// driver pushes [`BenchEvent`]s into.
pub enum Dispatcher {
    RustBaseline {
        sink: CommandSink,
        metrics: Arc<DispatcherMetrics>,
    },
    Lua {
        worker_count: usize,
        queues: Vec<Arc<WorkerQueue>>,
        handles: Vec<JoinHandle<Result<WorkerReport, String>>>,
        metrics: Arc<DispatcherMetrics>,
    },
}

impl Dispatcher {
    /// Starts the dispatcher. For `Architecture::Lua`, this spawns exactly
    /// `worker_count` dedicated OS threads up front — never more, never one
    /// per event or per bot beyond that fixed count (except Candidate F,
    /// where `worker_count == bot_count` by definition).
    pub fn start(
        architecture: Architecture,
        queue_kind: QueueKind,
        sandbox_config: SandboxConfig,
        script_body: &'static str,
        sink: CommandSink,
        metrics: Arc<DispatcherMetrics>,
    ) -> Self {
        match architecture {
            Architecture::RustBaseline => Dispatcher::RustBaseline { sink, metrics },
            Architecture::Lua { worker_count } => {
                let worker_count = worker_count.max(1);
                let mut queues = Vec::with_capacity(worker_count);
                let mut handles = Vec::with_capacity(worker_count);
                for _ in 0..worker_count {
                    let queue = WorkerQueue::new(build_queue(queue_kind));
                    queues.push(queue.clone());
                    let sink = sink.clone();
                    let m = metrics.clone();
                    let config = sandbox_config;
                    handles.push(std::thread::spawn(move || {
                        run_worker(queue, config, script_body, sink, m)
                    }));
                }
                Dispatcher::Lua {
                    worker_count,
                    queues,
                    handles,
                    metrics,
                }
            }
        }
    }

    /// Deterministic assignment: `bot_id % worker_count`. A bot is computed
    /// into the same worker index for the dispatcher's entire lifetime —
    /// nothing here ever migrates a bot between workers mid-run.
    fn worker_index_for(&self, bot_id: BotId) -> usize {
        match self {
            Dispatcher::RustBaseline { .. } => 0,
            Dispatcher::Lua { worker_count, .. } => bot_id.0 as usize % worker_count,
        }
    }

    pub fn dispatch(&self, envelope: Envelope) {
        match self {
            Dispatcher::RustBaseline { sink, metrics } => {
                rust_baseline_handle(envelope, sink, metrics);
            }
            Dispatcher::Lua { queues, .. } => {
                let index = self.worker_index_for(envelope.bot_id);
                queues[index].push(envelope);
            }
        }
    }

    pub fn queue_depth_total(&self) -> usize {
        match self {
            Dispatcher::RustBaseline { .. } => 0,
            Dispatcher::Lua { queues, .. } => queues.iter().map(|q| q.depth()).sum(),
        }
    }

    pub fn metrics(&self) -> Arc<DispatcherMetrics> {
        match self {
            Dispatcher::RustBaseline { metrics, .. } => metrics.clone(),
            Dispatcher::Lua { metrics, .. } => metrics.clone(),
        }
    }

    /// Closes every worker queue and joins every worker thread, returning
    /// their reports. Bounded by nothing but the workers' own drain time —
    /// callers wanting a hard timeout wrap this call accordingly.
    pub fn shutdown(self) -> Vec<Result<WorkerReport, String>> {
        match self {
            Dispatcher::RustBaseline { .. } => Vec::new(),
            Dispatcher::Lua {
                queues, handles, ..
            } => {
                for queue in &queues {
                    queue.close();
                }
                handles
                    .into_iter()
                    .map(|h| h.join().expect("worker thread panicked"))
                    .collect()
            }
        }
    }
}

/// Candidate A: the same per-event work the `realistic` Lua script does
/// (inspect the event, conditionally emit a command), implemented directly
/// in Rust with zero Lua involvement — the performance/memory floor every
/// Lua candidate is measured against.
fn rust_baseline_handle(envelope: Envelope, sink: &CommandSink, metrics: &DispatcherMetrics) {
    let start = std::time::Instant::now();
    metrics
        .enqueue_to_start
        .lock()
        .unwrap()
        .record(start.saturating_duration_since(envelope.enqueued_at));
    metrics
        .throughput
        .events_dispatched
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    metrics
        .throughput
        .handler_executions
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    if let BenchEvent::Chat { message, .. } = &envelope.event {
        if message == "move" {
            sink.emit(envelope.bot_id, BenchCommand::Forward(true), "chat");
            metrics
                .command_latency
                .lock()
                .unwrap()
                .record(envelope.enqueued_at.elapsed());
        }
    }

    metrics
        .enqueue_to_complete
        .lock()
        .unwrap()
        .record(start.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::scripts::ScriptKind;
    use std::time::Instant;

    fn env(bot: u32, event: BenchEvent) -> Envelope {
        Envelope {
            bot_id: BotId(bot),
            event,
            enqueued_at: Instant::now(),
            bot_seq: 0,
        }
    }

    #[test]
    fn worker_pool_assigns_each_bot_to_the_same_worker_every_time() {
        let (sink, _rx) = CommandSink::bounded(256);
        let metrics = Arc::new(DispatcherMetrics::new(1024));
        let dispatcher = Dispatcher::start(
            Architecture::Lua { worker_count: 4 },
            QueueKind::Fifo { capacity: 64 },
            SandboxConfig::default(),
            ScriptKind::LightState.source(),
            sink,
            metrics,
        );
        for bot in [0u32, 4, 8, 1, 5, 9] {
            assert_eq!(
                dispatcher.worker_index_for(BotId(bot)),
                bot as usize % 4,
                "bot {bot} must always map to the same worker index"
            );
        }
        dispatcher.shutdown();
    }

    #[test]
    fn rust_baseline_dispatches_without_any_lua_worker_threads() {
        let (sink, rx) = CommandSink::bounded(16);
        let metrics = Arc::new(DispatcherMetrics::new(64));
        let dispatcher = Dispatcher::start(
            Architecture::RustBaseline,
            QueueKind::Fifo { capacity: 8 },
            SandboxConfig::default(),
            ScriptKind::NoOp.source(),
            sink,
            metrics.clone(),
        );
        dispatcher.dispatch(env(
            0,
            BenchEvent::Chat {
                sender_len: 1,
                message: "move".to_string(),
            },
        ));
        assert!(rx.try_recv().is_ok());
        assert_eq!(
            metrics
                .throughput
                .events_dispatched
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let reports = dispatcher.shutdown();
        assert!(reports.is_empty());
    }

    #[test]
    fn shared_vm_processes_events_from_multiple_bots_through_one_worker() {
        let (sink, _rx) = CommandSink::bounded(256);
        let metrics = Arc::new(DispatcherMetrics::new(1024));
        let dispatcher = Dispatcher::start(
            Architecture::Lua { worker_count: 1 },
            QueueKind::Fifo { capacity: 256 },
            SandboxConfig::default(),
            ScriptKind::LightState.source(),
            sink,
            metrics.clone(),
        );
        for bot in 0..10u32 {
            dispatcher.dispatch(env(bot, BenchEvent::Connected));
        }
        let reports = dispatcher.shutdown();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].as_ref().unwrap().events_processed, 10);
    }

    #[test]
    fn per_bot_lua_architecture_creates_exactly_one_worker_per_bot() {
        let (sink, _rx) = CommandSink::bounded(64);
        let metrics = Arc::new(DispatcherMetrics::new(64));
        let bot_count = 6usize;
        let architecture = Architecture::parse("per-bot-lua", 0, bot_count).unwrap();
        let dispatcher = Dispatcher::start(
            architecture,
            QueueKind::Fifo { capacity: 8 },
            SandboxConfig::default(),
            ScriptKind::NoOp.source(),
            sink,
            metrics,
        );
        for bot in 0..bot_count as u32 {
            dispatcher.dispatch(env(bot, BenchEvent::Connected));
        }
        let reports = dispatcher.shutdown();
        assert_eq!(reports.len(), bot_count);
        for report in &reports {
            assert_eq!(report.as_ref().unwrap().events_processed, 1);
        }
    }
}
