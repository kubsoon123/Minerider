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

use crate::lua::worker::{invoke_top_level, record_bot_handler_outcome, WorkerReport, WorkerState};

pub const MAX_TIMERS_PER_WORKER: usize = 1024;
pub const MIN_TIMER_INTERVAL: Duration = Duration::from_millis(10);

pub struct TimerEntry {
    pub key: RegistryKey,
    pub interval: Option<Duration>,
    pub next_fire: Instant,
    /// `Some(bot_id)` for a bot-scoped timer (`bot:set_timeout`/`set_interval`,
    /// scheduled via [`schedule_for_bot`]), `None` for a global/coordinator
    /// timer (`swarm:set_timeout`/`set_interval`). Retained so a failing
    /// bot-scoped timer callback can be attributed to the right bot for
    /// consecutive-error tracking, exactly like a failed event handler —
    /// see `crate::lua::worker::record_bot_handler_outcome`.
    pub bot_id: Option<u32>,
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
    /// A script tried to register a bot-scoped timer for a bot owned by a
    /// *different* worker. Each worker owns a fully separate `Lua` VM, and
    /// a `mlua::RegistryKey` (what a timer's callback closure is stored
    /// as) is only ever valid within the VM that created it — there is no
    /// safe way to "move" the closure to the owning worker instead, so
    /// this is rejected rather than silently registered on the wrong
    /// worker (see `docs/lua_wrapper.md#worker-local-globals`).
    #[error(
        "cross_worker_timer: bot {bot_id} is owned by worker {owner}, not the calling worker {caller} — bot-scoped timers must be registered from that bot's own worker"
    )]
    CrossWorkerBot {
        bot_id: u32,
        owner: usize,
        caller: usize,
    },
}

/// Schedules a timer on this worker. `interval = None` fires once;
/// `Some(d)` repeats every `d`. `bot_id` is `None` for a global/coordinator
/// timer, `Some(id)` for a bot-scoped one (always via [`schedule_for_bot`]
/// below, which validates ownership first). Returns the timer id used by
/// `clear_timer`.
pub fn schedule(
    state: &Rc<WorkerState>,
    lua: &Lua,
    func: mlua::Function,
    delay: Duration,
    interval: Option<Duration>,
    bot_id: Option<u32>,
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
            bot_id,
        },
    );
    Ok(id)
}

/// Schedules a bot-scoped timer — identical to [`schedule`], but first
/// rejects the call if `bot_id` isn't actually owned by this worker (see
/// [`TimerError::CrossWorkerBot`]). The sole caller is
/// `crate::lua::api::bot::LuaBot`'s `set_timeout`/`set_interval` methods.
pub fn schedule_for_bot(
    state: &Rc<WorkerState>,
    bot_id: u32,
    lua: &Lua,
    func: mlua::Function,
    delay: Duration,
    interval: Option<Duration>,
) -> Result<u64, TimerError> {
    let owner = state.dispatcher.worker_index_for(bot_id);
    if owner != state.worker_index {
        return Err(TimerError::CrossWorkerBot {
            bot_id,
            owner,
            caller: state.worker_index,
        });
    }
    schedule(state, lua, func, delay, interval, Some(bot_id))
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
///
/// A bot-scoped timer whose bot has since been disabled (by too many
/// consecutive handler errors — see `crate::lua::worker::record_bot_handler_outcome`)
/// is skipped entirely: once a bot's script execution is disabled, *all*
/// of its script execution stops, not just its event handlers. A
/// bot-scoped timer's own success/failure feeds the same per-bot counter,
/// same as a failed/succeeded event handler.
pub fn fire_one(lua: &Lua, state: &Rc<WorkerState>, timer_id: u64, report: &mut WorkerReport) {
    let key_interval_bot = {
        let timers = state.timers.borrow();
        timers.entries.get(&timer_id).map(|e| {
            (
                lua.registry_value::<mlua::Function>(&e.key),
                e.interval,
                e.bot_id,
            )
        })
    };
    let Some((func_result, interval, bot_id)) = key_interval_bot else {
        return;
    };
    if let Some(id) = bot_id {
        if state.disabled_bots.borrow().contains(&id) {
            return;
        }
    }
    if let Ok(func) = func_result {
        report.handlers_run += 1;
        let outcome = invoke_top_level(state, &func, ());
        if let Err(e) = &outcome {
            report.handler_errors += 1;
            tracing::warn!(worker = state.worker_index, timer_id, error = %e, "timer callback failed");
        }
        if let Some(id) = bot_id {
            record_bot_handler_outcome(state, id, outcome.is_ok(), report);
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
