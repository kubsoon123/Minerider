//! Bounded, generic per-worker inbound queues.
//!
//! Promoted from the architecture benchmark (`docs/lua_runtime_benchmark.md`
//! recommends this exact design) and generalized over the item type so both
//! the production dispatcher (`crate::lua::dispatcher`, items are
//! [`crate::lua::event::WorkItem`]) and the benchmark harness
//! (`crate::lua_benchmark`, items are `BenchEvent`) share one implementation
//! instead of two parallel ones.
//!
//! - [`FifoQueue`]: one bounded FIFO — the naive baseline.
//! - [`PriorityQueue`]: a small never-silently-full high-priority lane, a
//!   `(bot, kind)`-keyed coalescing slot for repeated state, and a bounded
//!   low-priority lane for discrete, non-critical, non-coalescible traffic.
//!
//! Every push returns a [`PushOutcome`] so callers can track peak depth,
//! dropped, and coalesced counts without re-deriving them from raw queue
//! contents afterward.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// A bot's stable numeric identity, used only for queue routing/coalescing
/// keys here — the authoritative definition (and its relationship to
/// worker assignment) lives in `crate::lua::registry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BotId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Coalescible,
    Low,
}

/// Implemented by whatever item type a queue is instantiated over, so the
/// queue logic itself never needs to know the concrete event type.
pub trait QueueItem {
    fn priority(&self) -> Priority;
    /// Stable event-kind name, used as (part of) the coalescing key and for
    /// diagnostics. Must be constant per logical event kind (e.g. every
    /// `Health` event returns `"health"`, regardless of payload).
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone)]
pub struct Envelope<E> {
    pub bot_id: BotId,
    pub event: E,
    pub enqueued_at: Instant,
    pub bot_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// The queue was full; the oldest entry was evicted to make room.
    DroppedOldest,
    /// The queue was full and configured to reject rather than evict.
    DroppedNewest,
    /// Replaced a previously-coalesced, not-yet-delivered entry for the
    /// same `(bot, kind)` key — the old one never reaches a handler.
    Coalesced,
    /// The high-priority lane was full when a *critical* item (a lifecycle
    /// event, action result, or callback completion) was pushed — the new
    /// item was rejected, not an older pending one silently evicted, so
    /// already-queued critical items keep their relative order. Distinct
    /// from `DroppedOldest`/`DroppedNewest` (which apply to the
    /// low-priority lane, where losing state under load is an accepted,
    /// expected trade-off) specifically so callers can react — see
    /// `crate::lua::dispatcher::WorkerQueue::push`.
    CriticalOverflow,
    /// The queue has already been closed (see `WorkerQueue::close`) —
    /// pushing into a closed queue is always rejected outright, never
    /// silently accepted into a queue nothing will ever drain again.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    DropOldest,
    DropNewest,
}

/// Depth/drop bookkeeping shared by both queue designs.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueStats {
    pub peak_depth: usize,
    pub dropped: u64,
    pub coalesced: u64,
    pub accepted: u64,
}

/// One bounded FIFO queue — the naive baseline the priority design is
/// compared against. Every event, whatever its real-world priority, competes
/// for the same fixed capacity in arrival order.
pub struct FifoQueue<E> {
    capacity: usize,
    policy: OverflowPolicy,
    items: VecDeque<Envelope<E>>,
    stats: QueueStats,
}

impl<E> FifoQueue<E> {
    pub fn new(capacity: usize, policy: OverflowPolicy) -> Self {
        Self {
            capacity,
            policy,
            items: VecDeque::with_capacity(capacity.min(1024)),
            stats: QueueStats::default(),
        }
    }

    pub fn push(&mut self, envelope: Envelope<E>) -> PushOutcome {
        let outcome = if self.items.len() >= self.capacity {
            match self.policy {
                OverflowPolicy::DropOldest => {
                    self.items.pop_front();
                    self.items.push_back(envelope);
                    self.stats.dropped += 1;
                    PushOutcome::DroppedOldest
                }
                OverflowPolicy::DropNewest => {
                    self.stats.dropped += 1;
                    return PushOutcome::DroppedNewest;
                }
            }
        } else {
            self.items.push_back(envelope);
            self.stats.accepted += 1;
            PushOutcome::Accepted
        };
        self.stats.peak_depth = self.stats.peak_depth.max(self.items.len());
        outcome
    }

    pub fn pop(&mut self) -> Option<Envelope<E>> {
        self.items.pop_front()
    }

    /// Drains everything currently queued, in FIFO order — the `FifoQueue`
    /// counterpart to [`PriorityQueue::drain_ready`], so a worker loop can
    /// treat either design uniformly.
    pub fn drain_ready(&mut self) -> Vec<Envelope<E>> {
        self.items.drain(..).collect()
    }

    pub fn depth(&self) -> usize {
        self.items.len()
    }

    pub fn stats(&self) -> QueueStats {
        self.stats
    }
}

/// The recommended design: high-priority lifecycle events always get in
/// (bounded only as a last-resort safety valve), repeated state coalesces
/// to the latest value per `(bot, kind)`, and low-frequency/discrete
/// traffic gets its own bounded lane instead of competing with lifecycle
/// events for the same slots.
pub struct PriorityQueue<E: QueueItem> {
    high: VecDeque<Envelope<E>>,
    high_capacity: usize,
    coalesced: HashMap<(BotId, &'static str), Envelope<E>>,
    coalesce_order: VecDeque<(BotId, &'static str)>,
    low: VecDeque<Envelope<E>>,
    low_capacity: usize,
    stats_high: QueueStats,
    stats_coalesced: QueueStats,
    stats_low: QueueStats,
}

impl<E: QueueItem> PriorityQueue<E> {
    pub fn new(high_capacity: usize, low_capacity: usize) -> Self {
        Self {
            high: VecDeque::new(),
            high_capacity,
            coalesced: HashMap::new(),
            coalesce_order: VecDeque::new(),
            low: VecDeque::new(),
            low_capacity,
            stats_high: QueueStats::default(),
            stats_coalesced: QueueStats::default(),
            stats_low: QueueStats::default(),
        }
    }

    pub fn push(&mut self, envelope: Envelope<E>) -> PushOutcome {
        match envelope.event.priority() {
            Priority::High => {
                if self.high.len() >= self.high_capacity {
                    // A genuinely full high-priority lane means the whole
                    // pipeline is saturated far beyond design limits. Reject
                    // the new item rather than evicting an older pending
                    // one: every already-queued critical item (a lifecycle
                    // event, an action result, a callback completion) keeps
                    // its place and its relative order, and the caller gets
                    // a distinct outcome it can react to (see
                    // `crate::lua::dispatcher::WorkerQueue::push`) instead
                    // of an older item vanishing with no signal at all.
                    self.stats_high.dropped += 1;
                    PushOutcome::CriticalOverflow
                } else {
                    self.high.push_back(envelope);
                    self.stats_high.accepted += 1;
                    self.stats_high.peak_depth = self.stats_high.peak_depth.max(self.high.len());
                    PushOutcome::Accepted
                }
            }
            Priority::Coalescible => {
                let key = (envelope.bot_id, envelope.event.name());
                let replaced = self.coalesced.insert(key, envelope).is_some();
                if !replaced {
                    self.coalesce_order.push_back(key);
                    self.stats_coalesced.accepted += 1;
                } else {
                    self.stats_coalesced.coalesced += 1;
                }
                self.stats_coalesced.peak_depth =
                    self.stats_coalesced.peak_depth.max(self.coalesced.len());
                if replaced {
                    PushOutcome::Coalesced
                } else {
                    PushOutcome::Accepted
                }
            }
            Priority::Low => {
                if self.low.len() >= self.low_capacity {
                    self.low.pop_front();
                    self.stats_low.dropped += 1;
                    self.low.push_back(envelope);
                    self.stats_low.peak_depth = self.stats_low.peak_depth.max(self.low.len());
                    PushOutcome::DroppedOldest
                } else {
                    self.low.push_back(envelope);
                    self.stats_low.accepted += 1;
                    self.stats_low.peak_depth = self.stats_low.peak_depth.max(self.low.len());
                    PushOutcome::Accepted
                }
            }
        }
    }

    /// Drains everything ready for dispatch this cycle: all pending high
    /// events (in order), then every currently-coalesced latest value, then
    /// low-priority events — matching the priority order a real dispatch
    /// loop should use.
    pub fn drain_ready(&mut self) -> Vec<Envelope<E>> {
        let mut out =
            Vec::with_capacity(self.high.len() + self.coalesce_order.len() + self.low.len());
        out.extend(self.high.drain(..));
        while let Some(key) = self.coalesce_order.pop_front() {
            if let Some(envelope) = self.coalesced.remove(&key) {
                out.push(envelope);
            }
        }
        out.extend(self.low.drain(..));
        out
    }

    pub fn depth(&self) -> usize {
        self.high.len() + self.coalesced.len() + self.low.len()
    }

    /// `(high, coalesced, low)` stats, kept separate since they measure
    /// different things (drops matter for high/low; coalesced count matters
    /// for the coalescing lane and is expected, not a failure).
    pub fn stats(&self) -> (QueueStats, QueueStats, QueueStats) {
        (self.stats_high, self.stats_coalesced, self.stats_low)
    }
}

/// The two queue designs, behind one interface so a worker loop doesn't
/// need to know which is in effect. Production always uses `Priority`;
/// `Fifo` exists for the benchmark's architecture comparison.
pub enum QueueDesign<E: QueueItem> {
    Fifo(FifoQueue<E>),
    Priority(PriorityQueue<E>),
}

impl<E: QueueItem> QueueDesign<E> {
    pub fn push(&mut self, envelope: Envelope<E>) -> PushOutcome {
        match self {
            Self::Fifo(q) => q.push(envelope),
            Self::Priority(q) => q.push(envelope),
        }
    }

    pub fn drain_ready(&mut self) -> Vec<Envelope<E>> {
        match self {
            Self::Fifo(q) => q.drain_ready(),
            Self::Priority(q) => q.drain_ready(),
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Self::Fifo(q) => q.depth(),
            Self::Priority(q) => q.depth(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.depth() == 0
    }

    /// The highest combined depth this queue reached over its whole
    /// lifetime so far — tracked continuously by the underlying
    /// [`QueueStats`], not a point-in-time snapshot.
    pub fn peak_depth(&self) -> usize {
        match self {
            Self::Fifo(q) => q.stats().peak_depth,
            Self::Priority(q) => {
                let (high, coalesced, low) = q.stats();
                high.peak_depth + coalesced.peak_depth + low.peak_depth
            }
        }
    }

    /// Total events dropped due to overflow across every lane (high and
    /// low combined — see [`critical_overflow`](Self::critical_overflow)
    /// for the high lane alone).
    pub fn dropped(&self) -> u64 {
        match self {
            Self::Fifo(q) => q.stats().dropped,
            Self::Priority(q) => {
                let (high, _, low) = q.stats();
                high.dropped + low.dropped
            }
        }
    }

    /// Events lost specifically to *high-priority* lane saturation — a
    /// lifecycle event, action result, or callback completion that could
    /// not be delivered at all. Tracked separately from
    /// [`dropped`](Self::dropped) (which also includes ordinary
    /// low-priority overflow, an accepted, expected trade-off under load)
    /// because a critical drop is a materially more serious condition:
    /// see `crate::lua::dispatcher::WorkerQueue::push`, which reacts to it
    /// specifically. `Fifo` has no high/low distinction (it exists only as
    /// the benchmark's naive-baseline comparison, never used in
    /// production), so this is always `0` for it.
    pub fn critical_overflow(&self) -> u64 {
        match self {
            Self::Fifo(_) => 0,
            Self::Priority(q) => q.stats().0.dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum TestEvent {
        Critical,
        State(i32),
        Chatty,
    }

    impl QueueItem for TestEvent {
        fn priority(&self) -> Priority {
            match self {
                TestEvent::Critical => Priority::High,
                TestEvent::State(_) => Priority::Coalescible,
                TestEvent::Chatty => Priority::Low,
            }
        }

        fn name(&self) -> &'static str {
            match self {
                TestEvent::Critical => "critical",
                TestEvent::State(_) => "state",
                TestEvent::Chatty => "chatty",
            }
        }
    }

    fn envelope(bot: u32, event: TestEvent, seq: u64) -> Envelope<TestEvent> {
        Envelope {
            bot_id: BotId(bot),
            event,
            enqueued_at: Instant::now(),
            bot_seq: seq,
        }
    }

    #[test]
    fn fifo_bounded_queue_drops_oldest_when_configured() {
        let mut q = FifoQueue::new(2, OverflowPolicy::DropOldest);
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 0)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 1)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 2)),
            PushOutcome::DroppedOldest
        );
        assert_eq!(q.depth(), 2);
        assert_eq!(q.stats().dropped, 1);
        assert_eq!(q.pop().unwrap().bot_seq, 1);
        assert_eq!(q.pop().unwrap().bot_seq, 2);
    }

    #[test]
    fn fifo_bounded_queue_drops_newest_when_configured() {
        let mut q = FifoQueue::new(1, OverflowPolicy::DropNewest);
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 0)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 1)),
            PushOutcome::DroppedNewest
        );
        assert_eq!(q.pop().unwrap().bot_seq, 0);
        assert!(q.pop().is_none());
    }

    #[test]
    fn priority_queue_never_drops_high_priority_under_normal_load() {
        let mut q = PriorityQueue::new(256, 8);
        for i in 0..200 {
            q.push(envelope(0, TestEvent::Critical, i));
        }
        let (high, _, _) = q.stats();
        assert_eq!(high.dropped, 0);
        assert_eq!(high.accepted, 200);
    }

    #[test]
    fn priority_queue_coalesces_repeated_state_to_the_latest_value() {
        let mut q = PriorityQueue::new(16, 16);
        for i in 0..50 {
            q.push(envelope(0, TestEvent::State(i as i32), i));
        }
        let (_, coalesced, _) = q.stats();
        assert_eq!(coalesced.accepted, 1);
        assert_eq!(coalesced.coalesced, 49);
        let drained = q.drain_ready();
        assert_eq!(drained.len(), 1);
        match &drained[0].event {
            TestEvent::State(v) => assert_eq!(*v, 49),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn priority_queue_drain_order_is_high_then_coalesced_then_low() {
        let mut q = PriorityQueue::new(16, 16);
        q.push(envelope(0, TestEvent::Chatty, 0));
        q.push(envelope(0, TestEvent::State(1), 1));
        q.push(envelope(0, TestEvent::Critical, 2));
        let drained = q.drain_ready();
        assert_eq!(drained[0].event.name(), "critical");
        assert_eq!(drained[1].event.name(), "state");
        assert_eq!(drained[2].event.name(), "chatty");
    }

    #[test]
    fn priority_queue_low_lane_drops_oldest_and_never_touches_high() {
        let mut q = PriorityQueue::new(16, 2);
        q.push(envelope(0, TestEvent::Critical, 0));
        for i in 0..10 {
            q.push(envelope(0, TestEvent::Chatty, i));
        }
        let (high, _, low) = q.stats();
        assert_eq!(high.dropped, 0);
        assert!(low.dropped > 0);
    }

    #[test]
    fn priority_queue_high_lane_rejects_the_newest_item_when_full_not_an_older_one() {
        let mut q = PriorityQueue::new(2, 8);
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 0)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 1)),
            PushOutcome::Accepted
        );
        // The lane is now full (capacity 2) — a third critical item must
        // be rejected outright, never silently evicting one of the two
        // already-pending ones.
        assert_eq!(
            q.push(envelope(0, TestEvent::Critical, 2)),
            PushOutcome::CriticalOverflow
        );
        let (high, _, _) = q.stats();
        assert_eq!(
            high.dropped, 1,
            "exactly one critical overflow must be recorded"
        );
        assert_eq!(
            high.accepted, 2,
            "the two originally-accepted items must not be retroactively uncounted"
        );

        // Both original items — in their original order — must still be
        // exactly what drains, proving nothing was evicted to make room.
        let drained = q.drain_ready();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].bot_seq, 0);
        assert_eq!(drained[1].bot_seq, 1);
    }

    #[test]
    fn critical_overflow_is_tracked_separately_from_ordinary_low_lane_drops() {
        let mut design: QueueDesign<TestEvent> = QueueDesign::Priority(PriorityQueue::new(1, 1));
        // Fill, then overflow, the high lane.
        design.push(envelope(0, TestEvent::Critical, 0));
        assert_eq!(
            design.push(envelope(0, TestEvent::Critical, 1)),
            PushOutcome::CriticalOverflow
        );
        // Fill, then overflow, the low lane too — an ordinary, expected
        // drop that must NOT be counted as a critical overflow.
        design.push(envelope(0, TestEvent::Chatty, 2));
        design.push(envelope(0, TestEvent::Chatty, 3));

        assert_eq!(design.critical_overflow(), 1);
        assert_eq!(
            design.dropped(),
            2,
            "dropped() stays the combined total (1 critical + 1 low)"
        );
    }

    #[test]
    fn per_bot_event_order_is_preserved_within_the_same_priority_lane() {
        let mut q = FifoQueue::new(100, OverflowPolicy::DropOldest);
        for i in 0..10 {
            q.push(envelope(0, TestEvent::Critical, i));
        }
        let mut last = -1i64;
        while let Some(e) = q.pop() {
            assert!(e.bot_seq as i64 > last);
            last = e.bot_seq as i64;
        }
    }
}
