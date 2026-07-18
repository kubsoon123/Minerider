//! Bounded event queues, and the two designs "Event-priority experiment"
//! asks this benchmark to compare:
//!
//! - [`FifoQueue`]: one bounded FIFO, the naive default.
//! - [`PriorityQueue`]: a small never-silently-full high-priority lane, a
//!   `(bot, event kind)`-keyed coalescing slot for repeated state, and a
//!   bounded low-priority lane for the high-frequency, poll-only-in-the-
//!   real-design traffic (kept queueable here only so this benchmark can
//!   measure what a mandatory per-tick callback would have cost).
//!
//! Every push returns a [`PushOutcome`] so callers can bump the exact
//! counters "Queues" in the final report asks for (peak depth, dropped,
//! coalesced) without re-deriving them from raw queue contents afterward.

use std::collections::{HashMap, VecDeque};

use super::event::{BotId, Envelope, Priority};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    DropOldest,
    DropNewest,
}

/// Depth/drop bookkeeping shared by both queue designs, so the benchmark
/// reports identical fields ("current queue depth, peak queue depth,
/// dropped events, coalesced events") for either.
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
pub struct FifoQueue {
    capacity: usize,
    policy: OverflowPolicy,
    items: VecDeque<Envelope>,
    stats: QueueStats,
}

impl FifoQueue {
    pub fn new(capacity: usize, policy: OverflowPolicy) -> Self {
        Self {
            capacity,
            policy,
            items: VecDeque::with_capacity(capacity.min(1024)),
            stats: QueueStats::default(),
        }
    }

    pub fn push(&mut self, envelope: Envelope) -> PushOutcome {
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

    pub fn pop(&mut self) -> Option<Envelope> {
        self.items.pop_front()
    }

    /// Drains everything currently queued, in FIFO order — the `FifoQueue`
    /// counterpart to [`PriorityQueue::drain_ready`], so a worker loop can
    /// treat either design uniformly (see `worker.rs`).
    pub fn drain_ready(&mut self) -> Vec<Envelope> {
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
/// (bounded only as a last-resort safety valve — see
/// [`PriorityQueue::high_capacity`]), repeated state coalesces to the
/// latest value per `(bot, kind)`, and low-frequency/high-volume traffic
/// gets its own small, aggressively-dropped lane instead of competing with
/// lifecycle events for the same slots.
pub struct PriorityQueue {
    high: VecDeque<Envelope>,
    high_capacity: usize,
    coalesced: HashMap<(BotId, &'static str), Envelope>,
    coalesce_order: VecDeque<(BotId, &'static str)>,
    low: VecDeque<Envelope>,
    low_capacity: usize,
    stats_high: QueueStats,
    stats_coalesced: QueueStats,
    stats_low: QueueStats,
}

impl PriorityQueue {
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

    pub fn push(&mut self, envelope: Envelope) -> PushOutcome {
        match envelope.event.priority() {
            Priority::High => {
                if self.high.len() >= self.high_capacity {
                    // A genuinely full high-priority lane means the whole
                    // pipeline is saturated far beyond design limits; drop
                    // oldest rather than newest so the *current* lifecycle
                    // state (e.g. the latest reconnect attempt) always wins.
                    self.high.pop_front();
                    self.stats_high.dropped += 1;
                    self.high.push_back(envelope);
                    self.stats_high.peak_depth = self.stats_high.peak_depth.max(self.high.len());
                    PushOutcome::DroppedOldest
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
    /// loop would use.
    pub fn drain_ready(&mut self) -> Vec<Envelope> {
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

/// The two queue designs "Event-priority experiment" compares, behind one
/// interface so a worker loop (see `worker.rs`) doesn't need to know which
/// is in effect.
pub enum QueueDesign {
    Fifo(FifoQueue),
    Priority(PriorityQueue),
}

impl QueueDesign {
    pub fn push(&mut self, envelope: Envelope) -> PushOutcome {
        match self {
            Self::Fifo(q) => q.push(envelope),
            Self::Priority(q) => q.push(envelope),
        }
    }

    pub fn drain_ready(&mut self) -> Vec<Envelope> {
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
    /// [`QueueStats`], not a point-in-time snapshot (see
    /// `docs/lua_runtime_benchmark.md`'s "Queues" measurements).
    pub fn peak_depth(&self) -> usize {
        match self {
            Self::Fifo(q) => q.stats().peak_depth,
            Self::Priority(q) => {
                let (high, coalesced, low) = q.stats();
                high.peak_depth + coalesced.peak_depth + low.peak_depth
            }
        }
    }

    /// Total events dropped due to overflow across every lane.
    pub fn dropped(&self) -> u64 {
        match self {
            Self::Fifo(q) => q.stats().dropped,
            Self::Priority(q) => {
                let (high, _, low) = q.stats();
                high.dropped + low.dropped
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::event::BenchEvent;
    use std::time::Instant;

    fn envelope(bot: u32, event: BenchEvent, seq: u64) -> Envelope {
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
            q.push(envelope(0, BenchEvent::Connected, 0)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, BenchEvent::Connected, 1)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, BenchEvent::Connected, 2)),
            PushOutcome::DroppedOldest
        );
        assert_eq!(q.depth(), 2);
        assert_eq!(q.stats().dropped, 1);
        // The oldest (seq 0) was evicted; seq 1 and 2 remain in order.
        assert_eq!(q.pop().unwrap().bot_seq, 1);
        assert_eq!(q.pop().unwrap().bot_seq, 2);
    }

    #[test]
    fn fifo_bounded_queue_drops_newest_when_configured() {
        let mut q = FifoQueue::new(1, OverflowPolicy::DropNewest);
        assert_eq!(
            q.push(envelope(0, BenchEvent::Connected, 0)),
            PushOutcome::Accepted
        );
        assert_eq!(
            q.push(envelope(0, BenchEvent::Connected, 1)),
            PushOutcome::DroppedNewest
        );
        assert_eq!(q.pop().unwrap().bot_seq, 0);
        assert!(q.pop().is_none());
    }

    #[test]
    fn priority_queue_never_drops_high_priority_under_normal_load() {
        let mut q = PriorityQueue::new(256, 8);
        for i in 0..200 {
            q.push(envelope(0, BenchEvent::Connected, i));
        }
        let (high, _, _) = q.stats();
        assert_eq!(high.dropped, 0);
        assert_eq!(high.accepted, 200);
    }

    #[test]
    fn priority_queue_coalesces_repeated_state_to_the_latest_value() {
        let mut q = PriorityQueue::new(16, 16);
        for i in 0..50 {
            q.push(envelope(
                0,
                BenchEvent::Health {
                    health: i as f32,
                    food: 20,
                },
                i,
            ));
        }
        let (_, coalesced, _) = q.stats();
        assert_eq!(coalesced.accepted, 1);
        assert_eq!(coalesced.coalesced, 49);
        let drained = q.drain_ready();
        assert_eq!(drained.len(), 1);
        match &drained[0].event {
            BenchEvent::Health { health, .. } => assert_eq!(*health, 49.0),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn priority_queue_drain_order_is_high_then_coalesced_then_low() {
        let mut q = PriorityQueue::new(16, 16);
        q.push(envelope(
            0,
            BenchEvent::EntityTick {
                entity_id: 1,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
        ));
        q.push(envelope(
            0,
            BenchEvent::Health {
                health: 20.0,
                food: 20,
            },
            1,
        ));
        q.push(envelope(0, BenchEvent::Connected, 2));
        let drained = q.drain_ready();
        assert_eq!(drained[0].event.name(), "connected");
        assert_eq!(drained[1].event.name(), "health");
        assert_eq!(drained[2].event.name(), "entity_tick");
    }

    #[test]
    fn priority_queue_low_lane_drops_oldest_and_never_touches_high() {
        let mut q = PriorityQueue::new(16, 2);
        q.push(envelope(0, BenchEvent::Connected, 0));
        for i in 0..10 {
            q.push(envelope(
                0,
                BenchEvent::EntityTick {
                    entity_id: 1,
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                i,
            ));
        }
        let (high, _, low) = q.stats();
        assert_eq!(high.dropped, 0);
        assert!(low.dropped > 0);
    }

    #[test]
    fn per_bot_event_order_is_preserved_within_the_same_priority_lane() {
        let mut q = FifoQueue::new(100, OverflowPolicy::DropOldest);
        for i in 0..10 {
            q.push(envelope(0, BenchEvent::Connected, i));
        }
        let mut last = -1i64;
        while let Some(e) = q.pop() {
            assert!(e.bot_seq as i64 > last);
            last = e.bot_seq as i64;
        }
    }
}
