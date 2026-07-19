//! The production dispatcher: exactly `worker_count` persistent workers
//! (4 by default — see `docs/lua_runtime_benchmark.md`'s recommendation),
//! deterministic `bot_id % worker_count` routing, never a VM or thread per
//! bot or per event.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::lua::error::ScriptError;
use crate::lua::event::WorkItem;
use crate::lua::queue::{BotId, Envelope, PushOutcome, QueueDesign};
use crate::minecraft::event::BotEvent;

/// The outcome of a previously-enqueued action, delivered back into the
/// owning bot's worker queue as a high-priority `action_result` event, and
/// (if the caller registered one) to a one-shot callback.
#[derive(Debug, Clone)]
pub struct ActionResult {
    pub request_id: u64,
    pub bot_id: u32,
    pub outcome: ActionOutcome,
}

#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// The command was validated and handed to the connection layer. This
    /// means the packet was sent (or queued to be sent), never that the
    /// server has acknowledged it.
    DeliveredSent,
    Confirmed {
        state_id: i32,
    },
    Corrected {
        state_id: i32,
    },
    Error(ScriptError),
}

impl ActionOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            ActionOutcome::DeliveredSent => "delivered_sent",
            ActionOutcome::Confirmed { .. } => "confirmed",
            ActionOutcome::Corrected { .. } => "corrected",
            ActionOutcome::Error(e) => e.code,
        }
    }
}

/// The shared, thread-safe queue a worker's dedicated thread blocks on and
/// producers (the tokio runtime routing supervisor events, or another
/// worker's action-result callback) push into.
pub struct WorkerQueue {
    state: std::sync::Mutex<QueueDesign<WorkItem>>,
    condvar: std::sync::Condvar,
    closed: std::sync::atomic::AtomicBool,
    /// Critical (high-lane) overflows this queue has produced, tracked
    /// separately from `QueueDesign::dropped()`'s combined total — see
    /// `push`'s doc comment.
    critical_overflow: AtomicU64,
    /// `(request_id, error)` pairs for action results/callback completions
    /// that could not be delivered (critical overflow) or were rejected
    /// outright (pushed after `close()`). The owning worker drains this on
    /// its own thread via `take_failed_requests` and resolves the
    /// matching pending callback with the given typed error — this Mutex
    /// (not the Lua-VM-bound `WorkerState::callbacks`) is what lets the
    /// async/producer side record the failure at all, since it can never
    /// safely touch a worker's `Rc`-based callback registry directly.
    failed_requests: std::sync::Mutex<Vec<(u64, ScriptError)>>,
}

impl WorkerQueue {
    pub fn new(design: QueueDesign<WorkItem>) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(design),
            condvar: std::sync::Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
            critical_overflow: AtomicU64::new(0),
            failed_requests: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Rejects outright once the queue is [`close`](Self::close)d — never
    /// silently accepted into a queue nothing will ever drain again.
    ///
    /// A lost critical (high-lane) item is never silent either: this
    /// bumps a dedicated `critical_overflow` counter, and — for an
    /// `ActionResult`/`CallbackCompletion` specifically, since those are
    /// the only work items a pending Lua callback might be waiting on —
    /// records `(request_id, worker_overloaded)` (or `(request_id,
    /// shutdown)` for a post-close rejection) so the owning worker can
    /// resolve that callback with a typed error instead of leaving it to
    /// silently expire via the callback-timeout sweep. Also makes one
    /// best-effort, non-recursive attempt to deliver a `WorkerOverloaded`
    /// diagnostic — into the *low* lane, a separately-bounded lane
    /// unaffected by the high lane's saturation, and only ever this one
    /// attempt, never a retry of the push that just failed.
    pub fn push(&self, bot_id: BotId, event: WorkItem, bot_seq: u64) -> PushOutcome {
        // Captured *before* `event` is moved into `envelope` below — the
        // only way to still know what a lost item was without needing it
        // handed back from a failed `QueueDesign::push`.
        let resolvable_request_id = match &event {
            WorkItem::ActionResult(r) | WorkItem::CallbackCompletion(r) => Some(r.request_id),
            _ => None,
        };
        if self.closed.load(Ordering::Acquire) {
            if let Some(request_id) = resolvable_request_id {
                self.record_failure(request_id, ScriptError::shutdown());
            }
            return PushOutcome::Closed;
        }
        let envelope = Envelope {
            bot_id,
            event,
            enqueued_at: std::time::Instant::now(),
            bot_seq,
        };
        let outcome = {
            let mut guard = self.state.lock().expect("worker queue mutex poisoned");
            guard.push(envelope)
        };
        if outcome == PushOutcome::CriticalOverflow {
            self.critical_overflow.fetch_add(1, Ordering::Relaxed);
            if let Some(request_id) = resolvable_request_id {
                self.record_failure(request_id, ScriptError::worker_overloaded());
            }
            // Best-effort, non-recursive: exactly one attempt, into the
            // low lane (a separately-bounded lane the high lane's
            // saturation can't affect) — never a retry of the push that
            // just failed, and its own outcome is deliberately ignored
            // (if the low lane is also saturated, this is simply dropped
            // like any other low-priority traffic under sustained
            // overload, which is already an accepted trade-off).
            let mut guard = self.state.lock().expect("worker queue mutex poisoned");
            let _ = guard.push(Envelope {
                bot_id: BotId(0),
                event: WorkItem::WorkerOverloaded,
                enqueued_at: std::time::Instant::now(),
                bot_seq: 0,
            });
        }
        self.condvar.notify_one();
        outcome
    }

    fn record_failure(&self, request_id: u64, err: ScriptError) {
        self.failed_requests
            .lock()
            .expect("worker queue failed-requests mutex poisoned")
            .push((request_id, err));
    }

    /// Drains every `(request_id, error)` recorded by a lost/rejected
    /// action result or callback completion since the last call — see
    /// `push`'s doc comment. Called once per dispatch-loop iteration by
    /// the owning worker (`crate::lua::worker::run_worker`).
    pub fn take_failed_requests(&self) -> Vec<(u64, ScriptError)> {
        std::mem::take(
            &mut *self
                .failed_requests
                .lock()
                .expect("worker queue failed-requests mutex poisoned"),
        )
    }

    /// Critical (high-lane) items lost to saturation — see `push`'s doc
    /// comment. Distinct from `QueueDesign::dropped()`'s combined total.
    pub fn critical_overflow(&self) -> u64 {
        self.critical_overflow.load(Ordering::Relaxed)
    }

    pub fn push_action_result(&self, result: ActionResult) {
        self.push(BotId(result.bot_id), WorkItem::ActionResult(result), 0);
    }

    /// Blocks until at least one envelope is available, the queue is
    /// closed, or `timers::MIN_TIMER_INTERVAL`-ish time has passed (whichever
    /// first), then drains everything currently ready (high, then
    /// coalesced, then low — see `crate::lua::queue::PriorityQueue`). The
    /// timeout is what lets an idle worker still check its due timers
    /// promptly (see `crate::lua::api::timers::fire_due`) instead of
    /// blocking forever with no events.
    pub fn wait_for_batch(&self) -> Vec<Envelope<WorkItem>> {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
        let mut guard = self.state.lock().expect("worker queue mutex poisoned");
        loop {
            if !guard.is_empty() || self.closed.load(Ordering::Acquire) {
                return guard.drain_ready();
            }
            let (next_guard, timeout) = self
                .condvar
                .wait_timeout(guard, POLL_INTERVAL)
                .expect("worker queue condvar poisoned");
            guard = next_guard;
            if timeout.timed_out() && guard.is_empty() {
                return Vec::new();
            }
        }
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.condvar.notify_all();
    }

    pub fn depth(&self) -> usize {
        self.state
            .lock()
            .expect("worker queue mutex poisoned")
            .depth()
    }

    pub fn peak_depth(&self) -> usize {
        self.state
            .lock()
            .expect("worker queue mutex poisoned")
            .peak_depth()
    }

    pub fn dropped(&self) -> u64 {
        self.state
            .lock()
            .expect("worker queue mutex poisoned")
            .dropped()
    }
}

/// Handle-side view of the dispatcher: everything needed to route work
/// into the right worker's queue and allocate request ids. Cheap to clone
/// and share across the tokio runtime tasks that bridge supervisor events
/// into worker queues.
#[derive(Clone)]
pub struct DispatcherHandle {
    queues: Arc<Vec<Arc<WorkerQueue>>>,
    next_request_id: Arc<AtomicU64>,
    per_bot_seq: Arc<std::sync::Mutex<std::collections::HashMap<u32, u64>>>,
}

impl DispatcherHandle {
    pub fn new(queues: Vec<Arc<WorkerQueue>>) -> Self {
        Self {
            queues: Arc::new(queues),
            next_request_id: Arc::new(AtomicU64::new(1)),
            per_bot_seq: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.queues.len()
    }

    pub fn worker_index_for(&self, bot_id: u32) -> usize {
        (bot_id as usize) % self.queues.len()
    }

    fn next_bot_seq(&self, bot_id: u32) -> u64 {
        let mut guard = self.per_bot_seq.lock().expect("per-bot seq mutex poisoned");
        let seq = guard.entry(bot_id).or_insert(0);
        *seq += 1;
        *seq
    }

    pub fn allocate_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn dispatch_bot_event(&self, bot_id: u32, event: BotEvent) -> PushOutcome {
        let idx = self.worker_index_for(bot_id);
        let seq = self.next_bot_seq(bot_id);
        self.queues[idx].push(BotId(bot_id), WorkItem::Bot(event), seq)
    }

    /// Delivers an action's completion to the bot's *owning* worker (for
    /// `action_result` handler dispatch, and — when `origin_worker` is
    /// that same owning worker — the common-case callback lookup too), and
    /// additionally, whenever `origin_worker` differs from the owning
    /// worker, delivers a second `CallbackCompletion` directly to
    /// `origin_worker` so a one-shot callback registered there (by
    /// `swarm:bot(id):click_gui(...)`-style calls made from a worker that
    /// doesn't own `bot_id`) is never silently lost. `origin_worker` is
    /// the worker whose Lua VM issued the action — see
    /// `crate::lua::api::bot::spawn_action`, the only caller.
    pub fn dispatch_action_result(&self, origin_worker: usize, result: ActionResult) {
        let owner = self.worker_index_for(result.bot_id);
        if origin_worker == owner {
            self.queues[owner].push_action_result(result);
            return;
        }
        self.queues[owner].push_action_result(result.clone());
        self.queues[origin_worker].push(
            BotId(result.bot_id),
            WorkItem::CallbackCompletion(result),
            0,
        );
    }

    pub fn dispatch_timer(&self, bot_id: u32, timer_id: u64) {
        let idx = self.worker_index_for(bot_id);
        let seq = self.next_bot_seq(bot_id);
        self.queues[idx].push(BotId(bot_id), WorkItem::TimerFired { timer_id }, seq);
    }

    /// Delivers a pub/sub message to every worker (each worker filters by
    /// its own `swarm:on_message` registrations). The payload is cloned
    /// once per worker so no two workers ever share a reference.
    pub fn broadcast_message(&self, topic: String, payload: crate::lua::shared_value::SharedValue) {
        for queue in self.queues.iter() {
            queue.push(
                BotId(0),
                WorkItem::Message {
                    topic: topic.clone(),
                    payload: payload.clone(),
                },
                0,
            );
        }
    }

    pub fn queue(&self, worker_index: usize) -> &Arc<WorkerQueue> {
        &self.queues[worker_index]
    }

    pub fn queue_depth_total(&self) -> usize {
        self.queues.iter().map(|q| q.depth()).sum()
    }

    pub fn queue_peak_depth_total(&self) -> usize {
        self.queues.iter().map(|q| q.peak_depth()).sum()
    }

    pub fn queue_dropped_total(&self) -> u64 {
        self.queues.iter().map(|q| q.dropped()).sum()
    }

    /// Critical (high-lane) items lost to saturation, across every worker
    /// — a distinct, more serious signal than `queue_dropped_total`'s
    /// combined (high + ordinary low-priority) total. Exposed via
    /// `swarm:stats()` as `queue_critical_overflow_total`.
    pub fn queue_critical_overflow_total(&self) -> u64 {
        self.queues.iter().map(|q| q.critical_overflow()).sum()
    }

    pub fn close_all(&self) {
        for queue in self.queues.iter() {
            queue.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::queue::{OverflowPolicy, PriorityQueue, QueueDesign};

    fn make_queues(n: usize) -> Vec<Arc<WorkerQueue>> {
        (0..n)
            .map(|_| WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(64, 64))))
            .collect()
    }

    #[test]
    fn worker_index_is_deterministic_and_stable() {
        let handle = DispatcherHandle::new(make_queues(4));
        for bot_id in 0..20u32 {
            let idx1 = handle.worker_index_for(bot_id);
            let idx2 = handle.worker_index_for(bot_id);
            assert_eq!(idx1, idx2);
            assert_eq!(idx1, (bot_id as usize) % 4);
        }
    }

    #[test]
    fn dispatch_bot_event_routes_to_the_assigned_worker_only() {
        let handle = DispatcherHandle::new(make_queues(4));
        handle.dispatch_bot_event(5, BotEvent::Connected);
        assert_eq!(handle.queue(1).depth(), 1);
        for i in [0, 2, 3] {
            assert_eq!(handle.queue(i).depth(), 0);
        }
    }

    #[test]
    fn dispatch_action_result_delivers_only_to_the_owner_when_origin_matches() {
        let handle = DispatcherHandle::new(make_queues(4));
        let result = ActionResult {
            request_id: 1,
            bot_id: 5,
            outcome: ActionOutcome::DeliveredSent,
        };
        // bot 5 is owned by worker 1 (5 % 4); origin == owner is the
        // common case (a script acting on its own bot) — must stay a
        // single push, not double up on every action.
        handle.dispatch_action_result(1, result);
        assert_eq!(
            handle.queue(1).depth(),
            1,
            "owning worker gets the ActionResult"
        );
        for i in [0, 2, 3] {
            assert_eq!(handle.queue(i).depth(), 0, "worker {i} must get nothing");
        }
    }

    #[test]
    fn dispatch_action_result_also_delivers_a_callback_completion_when_origin_differs_from_the_owner(
    ) {
        let handle = DispatcherHandle::new(make_queues(4));
        let result = ActionResult {
            request_id: 2,
            bot_id: 5,
            outcome: ActionOutcome::DeliveredSent,
        };
        // bot 5 is owned by worker 1; a different worker (2) is the
        // caller, e.g. via `swarm:bot(5):click_gui(...)` from worker 2.
        handle.dispatch_action_result(2, result);
        assert_eq!(
            handle.queue(1).depth(),
            1,
            "the owning worker still gets the ActionResult for its action_result handlers"
        );
        assert_eq!(
            handle.queue(2).depth(),
            1,
            "the origin worker gets a CallbackCompletion for its own pending callback"
        );
        for i in [0, 3] {
            assert_eq!(handle.queue(i).depth(), 0, "worker {i} must get nothing");
        }
        assert!(matches!(
            handle.queue(2).wait_for_batch()[0].event,
            WorkItem::CallbackCompletion(_)
        ));
        assert!(matches!(
            handle.queue(1).wait_for_batch()[0].event,
            WorkItem::ActionResult(_)
        ));
    }

    #[test]
    fn request_ids_are_unique_and_increasing() {
        let handle = DispatcherHandle::new(make_queues(1));
        let a = handle.allocate_request_id();
        let b = handle.allocate_request_id();
        assert!(b > a);
    }

    #[test]
    fn fifo_queue_variant_also_works_through_worker_queue() {
        let queue = WorkerQueue::new(QueueDesign::Fifo(crate::lua::queue::FifoQueue::new(
            8,
            OverflowPolicy::DropOldest,
        )));
        queue.push(BotId(0), WorkItem::Bot(BotEvent::Connected), 1);
        let batch = queue.wait_for_batch();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn close_unblocks_a_waiting_batch_with_an_empty_drain() {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(8, 8)));
        let queue2 = queue.clone();
        let handle = std::thread::spawn(move || queue2.wait_for_batch());
        std::thread::sleep(std::time::Duration::from_millis(50));
        queue.close();
        let batch = handle.join().unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn push_rejects_outright_once_the_queue_is_closed() {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(8, 8)));
        assert_eq!(
            queue.push(BotId(0), WorkItem::Bot(BotEvent::Connected), 1),
            PushOutcome::Accepted
        );
        queue.close();
        assert_eq!(
            queue.push(BotId(0), WorkItem::Bot(BotEvent::Connected), 2),
            PushOutcome::Closed,
            "a push after close must never be silently accepted"
        );
        assert_eq!(
            queue.depth(),
            1,
            "the post-close push must not have been added to the queue"
        );
    }

    #[test]
    fn critical_overflow_resolves_a_pending_action_result_with_a_typed_worker_overloaded_error() {
        // High-lane capacity 1: the first ActionResult fits, the second
        // overflows it.
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(1, 8)));
        queue.push_action_result(ActionResult {
            request_id: 1,
            bot_id: 0,
            outcome: ActionOutcome::DeliveredSent,
        });
        queue.push_action_result(ActionResult {
            request_id: 2,
            bot_id: 0,
            outcome: ActionOutcome::DeliveredSent,
        });

        assert_eq!(queue.critical_overflow(), 1);
        let failed = queue.take_failed_requests();
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].0, 2,
            "the request that overflowed, not the one that fit"
        );
        assert_eq!(failed[0].1.code, "worker_overloaded");

        assert!(
            queue.take_failed_requests().is_empty(),
            "take_failed_requests must drain, not just peek"
        );
    }

    #[test]
    fn critical_overflow_also_delivers_a_worker_overloaded_notification_into_the_low_lane() {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(1, 8)));
        queue.push_action_result(ActionResult {
            request_id: 1,
            bot_id: 0,
            outcome: ActionOutcome::DeliveredSent,
        });
        queue.push_action_result(ActionResult {
            request_id: 2,
            bot_id: 0,
            outcome: ActionOutcome::DeliveredSent,
        });

        let batch = queue.wait_for_batch();
        assert!(
            batch
                .iter()
                .any(|e| matches!(e.event, WorkItem::WorkerOverloaded)),
            "a WorkerOverloaded diagnostic must be delivered alongside the surviving ActionResult"
        );
    }

    #[test]
    fn push_after_close_resolves_a_pending_action_result_with_a_typed_shutdown_error() {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(8, 8)));
        queue.close();
        queue.push_action_result(ActionResult {
            request_id: 42,
            bot_id: 0,
            outcome: ActionOutcome::DeliveredSent,
        });

        let failed = queue.take_failed_requests();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, 42);
        assert_eq!(failed[0].1.code, "shutdown");
    }

    #[test]
    fn a_bot_event_lost_to_critical_overflow_is_counted_but_has_no_callback_to_resolve() {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(1, 8)));
        queue.push(BotId(0), WorkItem::Bot(BotEvent::Connected), 1);
        let outcome = queue.push(
            BotId(0),
            WorkItem::Bot(BotEvent::Disconnected {
                reason: "test".to_string(),
            }),
            2,
        );

        assert_eq!(outcome, PushOutcome::CriticalOverflow);
        assert_eq!(queue.critical_overflow(), 1);
        assert!(
            queue.take_failed_requests().is_empty(),
            "a lost lifecycle event has no request_id/callback to resolve"
        );
    }
}
