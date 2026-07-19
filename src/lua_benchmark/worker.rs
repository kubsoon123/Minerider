//! One persistent Lua worker: one `Lua` VM, one dedicated OS thread, a
//! shared bounded inbound queue. Never a VM per event, never a task per
//! event, never concurrent Lua execution against the same VM (see "Runtime
//! design rules") — the worker loop below processes exactly one envelope's
//! handlers to completion before looking at the next.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use super::command::CommandSink;
use super::event::{BenchEvent, BotId, Envelope};
use super::lua_api::{
    event_to_table, handlers_for, install, DispatchContext, SharedDispatchContext,
};
use super::metrics::DispatcherMetrics;
use super::queue::{PushOutcome, QueueDesign};
use super::sandbox::{new_sandboxed_lua, SandboxConfig};

/// The shared, thread-safe queue a worker's dedicated thread blocks on and
/// producers push into. Wraps a [`QueueDesign`] (FIFO or priority) behind a
/// `Mutex` + `Condvar` so pushing never needs the worker thread awake, and
/// the worker thread never busy-polls.
pub struct WorkerQueue {
    state: Mutex<QueueDesign<BenchEvent>>,
    condvar: Condvar,
    closed: std::sync::atomic::AtomicBool,
}

impl WorkerQueue {
    pub fn new(design: QueueDesign<BenchEvent>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(design),
            condvar: Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn push(&self, envelope: Envelope) -> PushOutcome {
        let mut guard = self.state.lock().unwrap();
        let outcome = guard.push(envelope);
        drop(guard);
        self.condvar.notify_one();
        outcome
    }

    pub fn depth(&self) -> usize {
        self.state.lock().unwrap().depth()
    }

    pub fn peak_depth(&self) -> usize {
        self.state.lock().unwrap().peak_depth()
    }

    pub fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped()
    }

    /// No more pushes will happen; wakes the worker so it can drain
    /// whatever remains and exit.
    pub fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.condvar.notify_all();
    }

    /// Blocks until there is work or the queue is closed and drained,
    /// returning the next batch (possibly empty, which signals shutdown).
    fn wait_for_batch(&self) -> Vec<Envelope> {
        let mut guard = self.state.lock().unwrap();
        loop {
            if !guard.is_empty() {
                return guard.drain_ready();
            }
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Vec::new();
            }
            guard = self.condvar.wait(guard).unwrap();
        }
    }
}

#[derive(Debug, Default)]
pub struct WorkerReport {
    pub events_processed: u64,
    pub handlers_run: u64,
    pub handler_errors: u64,
    pub bots_disabled_by_consecutive_errors: u64,
}

/// Runs one worker to completion (until its queue is closed and drained),
/// returning a report. Spawn this on a dedicated `std::thread` — never a
/// tokio task — so the `Lua` it owns never crosses an `.await` and never
/// needs to be `Send` (see `docs/lua_runtime_benchmark.md`'s threading
/// audit). The error type is a plain `String`, not `mlua::Error`: an
/// `mlua::Error` is itself not `Send` without the `send` feature this crate
/// deliberately does not enable, and `std::thread::spawn` always requires a
/// spawned closure's return type to be `Send` to hand back through its
/// `JoinHandle` — converting at this one boundary keeps that requirement
/// satisfied without making `Lua` itself cross threads.
pub fn run_worker(
    queue: Arc<WorkerQueue>,
    sandbox_config: SandboxConfig,
    script_body: &str,
    command_sink: CommandSink,
    metrics: Arc<DispatcherMetrics>,
) -> Result<WorkerReport, String> {
    let (lua, instruction_counter) =
        new_sandboxed_lua(&sandbox_config).map_err(|e| e.to_string())?;
    let context: SharedDispatchContext = Arc::new(Mutex::new(DispatchContext {
        event_name: "connected",
        envelope_enqueued_at: Instant::now(),
    }));
    install(
        &lua,
        command_sink,
        context.clone(),
        metrics.command_latency.clone(),
        script_body,
    )
    .map_err(|e| e.to_string())?;

    let mut consecutive_errors: HashMap<BotId, u32> = HashMap::new();
    let mut disabled: std::collections::HashSet<BotId> = std::collections::HashSet::new();
    let mut report = WorkerReport::default();

    loop {
        let batch = queue.wait_for_batch();
        if batch.is_empty() {
            break;
        }
        for envelope in batch {
            report.events_processed += 1;
            metrics
                .throughput
                .events_dispatched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if disabled.contains(&envelope.bot_id) {
                continue;
            }

            let start = Instant::now();
            metrics
                .enqueue_to_start
                .lock()
                .unwrap()
                .record(start.saturating_duration_since(envelope.enqueued_at));

            *context.lock().unwrap() = DispatchContext {
                event_name: envelope.event.name(),
                envelope_enqueued_at: envelope.enqueued_at,
            };
            instruction_counter.store(0, std::sync::atomic::Ordering::Relaxed);

            let table = match event_to_table(&lua, &envelope.event) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let handlers = handlers_for(&lua, envelope.event.name()).unwrap_or_default();
            let mut had_error = false;
            for handler in handlers {
                report.handlers_run += 1;
                metrics
                    .throughput
                    .handler_executions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                instruction_counter.store(0, std::sync::atomic::Ordering::Relaxed);
                let result: mlua::Result<()> = handler.call((envelope.bot_id.0, table.clone()));
                if result.is_err() {
                    had_error = true;
                    report.handler_errors += 1;
                    metrics
                        .throughput
                        .handler_errors
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }

            metrics
                .enqueue_to_complete
                .lock()
                .unwrap()
                .record(start.elapsed());

            let count = consecutive_errors.entry(envelope.bot_id).or_insert(0);
            if had_error {
                *count += 1;
                if *count >= sandbox_config.consecutive_error_threshold {
                    disabled.insert(envelope.bot_id);
                    report.bots_disabled_by_consecutive_errors += 1;
                }
            } else {
                *count = 0;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::command::CommandSink;
    use crate::lua_benchmark::event::BenchEvent;
    use crate::lua_benchmark::queue::{FifoQueue, OverflowPolicy};
    use crate::lua_benchmark::scripts::ScriptKind;

    #[test]
    fn worker_processes_pushed_events_and_shuts_down_cleanly_on_close() {
        let queue = WorkerQueue::new(QueueDesign::Fifo(FifoQueue::new(
            64,
            OverflowPolicy::DropOldest,
        )));
        let (sink, _rx) = CommandSink::bounded(16);
        let metrics = Arc::new(DispatcherMetrics::new(1024));

        for i in 0..5 {
            queue.push(Envelope {
                bot_id: BotId(i),
                event: BenchEvent::Connected,
                enqueued_at: Instant::now(),
                bot_seq: 0,
            });
        }
        queue.close();

        let report = run_worker(
            queue,
            SandboxConfig::default(),
            ScriptKind::LightState.source(),
            sink,
            metrics.clone(),
        )
        .unwrap();

        assert_eq!(report.events_processed, 5);
        assert_eq!(metrics.throughput.snapshot().events_dispatched, 5);
    }

    #[test]
    fn a_slow_handler_does_not_prevent_the_worker_from_finishing() {
        let queue = WorkerQueue::new(QueueDesign::Fifo(FifoQueue::new(
            8,
            OverflowPolicy::DropOldest,
        )));
        let (sink, _rx) = CommandSink::bounded(16);
        let metrics = Arc::new(DispatcherMetrics::new(1024));
        queue.push(Envelope {
            bot_id: BotId(0),
            event: BenchEvent::Health {
                health: 20.0,
                food: 20,
            },
            enqueued_at: Instant::now(),
            bot_seq: 0,
        });
        queue.close();
        let report = run_worker(
            queue,
            SandboxConfig::default(),
            ScriptKind::SlowHandler.source(),
            sink,
            metrics,
        )
        .unwrap();
        assert_eq!(report.events_processed, 1);
        assert_eq!(report.handler_errors, 0);
    }

    #[test]
    fn consecutive_handler_errors_disable_the_offending_bot_only() {
        let queue = WorkerQueue::new(QueueDesign::Fifo(FifoQueue::new(
            64,
            OverflowPolicy::DropOldest,
        )));
        let (sink, _rx) = CommandSink::bounded(16);
        let metrics = Arc::new(DispatcherMetrics::new(1024));
        let config = SandboxConfig {
            instruction_budget: 50,
            hook_every_n_instructions: 10,
            consecutive_error_threshold: 3,
            ..SandboxConfig::default()
        };
        // Both bots hit the same global infinite-loop handler (handlers are
        // not per-bot), but only bot 0 sends enough events to cross the
        // consecutive-error threshold; bot 1's single error must not count
        // toward bot 0's streak or get bot 1 disabled too.
        for _ in 0..5 {
            queue.push(Envelope {
                bot_id: BotId(0),
                event: BenchEvent::Connected,
                enqueued_at: Instant::now(),
                bot_seq: 0,
            });
        }
        queue.push(Envelope {
            bot_id: BotId(1),
            event: BenchEvent::Connected,
            enqueued_at: Instant::now(),
            bot_seq: 0,
        });
        queue.close();
        let report = run_worker(
            queue,
            config,
            ScriptKind::InfiniteLoop.source(),
            sink,
            metrics,
        )
        .unwrap();
        assert!(report.handler_errors >= 3);
        assert_eq!(report.bots_disabled_by_consecutive_errors, 1);
    }
}
