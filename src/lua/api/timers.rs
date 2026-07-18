//! `swarm:set_timeout/set_interval/clear_timer`, and the bot-scoped
//! `bot:set_timeout/set_interval` equivalents that run on that bot's own
//! worker. One timer scheduler per worker (never a task per timer),
//! bounded timer count, a minimum interval, and no drift guarantees beyond
//! "checked at least once per queue-wait timeout" (see
//! `crate::lua::dispatcher::WorkerQueue::wait_for_batch`, which wakes
//! periodically even with an empty queue specifically so timers are
//! checked promptly).
//!
//! Coordinator/global timers (`swarm:set_timeout`, not `bot:set_timeout`)
//! only actually arm on worker 0 — every worker runs the same script, so
//! without this restriction a "global" timer would fire once per worker
//! instead of exactly once.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{Lua, RegistryKey};

use crate::lua::worker::{WorkerReport, WorkerState};

pub const MAX_TIMERS_PER_WORKER: usize = 1024;
pub const MIN_TIMER_INTERVAL: Duration = Duration::from_millis(10);

pub struct TimerEntry {
    pub key: RegistryKey,
    pub interval: Option<Duration>,
    pub next_fire: Instant,
}

#[derive(Default)]
pub struct TimerRegistry {
    pub entries: HashMap<u64, TimerEntry>,
    pub next_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TimerError {
    #[error("worker timer limit of {MAX_TIMERS_PER_WORKER} exceeded")]
    LimitExceeded,
}

/// Schedules a timer on this worker. `interval = None` fires once;
/// `Some(d)` repeats every `d`. Returns the timer id used by
/// `clear_timer`.
pub fn schedule(
    state: &Rc<WorkerState>,
    lua: &Lua,
    func: mlua::Function,
    delay: Duration,
    interval: Option<Duration>,
) -> Result<u64, TimerError> {
    let delay = delay.max(MIN_TIMER_INTERVAL);
    let interval = interval.map(|i| i.max(MIN_TIMER_INTERVAL));
    let mut timers = state.timers.borrow_mut();
    if timers.entries.len() >= MAX_TIMERS_PER_WORKER {
        return Err(TimerError::LimitExceeded);
    }
    timers.next_id += 1;
    let id = timers.next_id;
    let key = lua
        .create_registry_value(func)
        .expect("registry value creation should not fail for a plain function");
    timers.entries.insert(
        id,
        TimerEntry {
            key,
            interval,
            next_fire: Instant::now() + delay,
        },
    );
    Ok(id)
}

pub fn clear(state: &Rc<WorkerState>, lua: &Lua, id: u64) -> bool {
    if let Some(entry) = state.timers.borrow_mut().entries.remove(&id) {
        let _ = lua.remove_registry_value(entry.key);
        true
    } else {
        false
    }
}

/// Cancels every timer on this worker — called on shutdown.
pub fn clear_all(state: &Rc<WorkerState>, lua: &Lua) {
    let ids: Vec<u64> = state.timers.borrow().entries.keys().copied().collect();
    for id in ids {
        clear(state, lua, id);
    }
}

/// Checks for and fires every due timer on this worker. Called once per
/// dispatch loop iteration (after draining the queue's ready batch, and
/// after every timed-out wait — see `worker::run_worker`).
pub fn fire_due(lua: &Lua, state: &Rc<WorkerState>, report: &mut WorkerReport) {
    let now = Instant::now();
    let due: Vec<u64> = {
        let timers = state.timers.borrow();
        timers
            .entries
            .iter()
            .filter(|(_, e)| e.next_fire <= now)
            .map(|(id, _)| *id)
            .collect()
    };
    for id in due {
        fire_one(lua, state, id, report);
    }
}

/// Fires exactly one timer by id, rescheduling it if it repeats. Public so
/// a future cross-worker timer-dispatch path (`WorkItem::TimerFired`) can
/// reuse the same firing logic as the direct per-worker check above.
pub fn fire_one(lua: &Lua, state: &Rc<WorkerState>, timer_id: u64, report: &mut WorkerReport) {
    let key_and_interval = {
        let timers = state.timers.borrow();
        timers.entries.get(&timer_id).map(|e| (lua.registry_value::<mlua::Function>(&e.key), e.interval))
    };
    let Some((func_result, interval)) = key_and_interval else {
        return;
    };
    if let Ok(func) = func_result {
        report.handlers_run += 1;
        if let Err(e) = func.call::<()>(()) {
            report.handler_errors += 1;
            tracing::warn!(worker = state.worker_index, timer_id, error = %e, "timer callback failed");
        }
    }
    let mut timers = state.timers.borrow_mut();
    match interval {
        Some(interval) => {
            if let Some(entry) = timers.entries.get_mut(&timer_id) {
                entry.next_fire = Instant::now() + interval;
            }
        }
        None => {
            if let Some(entry) = timers.entries.remove(&timer_id) {
                let _ = lua.remove_registry_value(entry.key);
            }
        }
    }
}

pub fn invoke(lua: &Lua, state: &Rc<WorkerState>, timer_id: u64, report: &mut WorkerReport) {
    fire_one(lua, state, timer_id, report);
}
