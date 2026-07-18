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
    Confirmed { state_id: i32 },
    Corrected { state_id: i32 },
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
}

impl WorkerQueue {
    pub fn new(design: QueueDesign<WorkItem>) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(design),
            condvar: std::sync::Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn push(&self, bot_id: BotId, event: WorkItem, bot_seq: u64) -> PushOutcome {
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
        self.condvar.notify_one();
        outcome
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
        self.state.lock().expect("worker queue mutex poisoned").depth()
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

    pub fn dispatch_action_result(&self, result: ActionResult) {
        let idx = self.worker_index_for(result.bot_id);
        self.queues[idx].push_action_result(result);
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
}
