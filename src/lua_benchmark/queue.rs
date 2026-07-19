//! Re-exports the production queue implementation (`crate::lua::queue`) so
//! the benchmark measures the exact queue design the production dispatcher
//! ships, rather than a second parallel implementation. See
//! `crate::lua::queue` for the actual types and `docs/lua_runtime_benchmark.md`
//! for the design rationale ("Event-priority experiment").

pub use crate::lua::queue::{
    BotId, Envelope, FifoQueue, OverflowPolicy, Priority, PriorityQueue, PushOutcome, QueueDesign,
    QueueItem, QueueStats,
};
