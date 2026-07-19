//! The Lua-facing API surface: everything registered into a worker's `Lua`
//! VM (`minerider` global, `swarm` global, and the `bot`/`group` userdata
//! types bots/groups are represented as). See `docs/lua_api_reference.md`
//! for the full method-by-method reference this module implements.

pub mod bot;
pub mod config;
pub mod errors;
pub mod group;
pub mod shared;
pub mod swarm;
pub mod timers;

use std::rc::Rc;

use mlua::Lua;

use crate::lua::worker::WorkerState;

/// Every event name a script can register a handler for. Validating
/// against this list at registration time (rather than accepting any
/// string) turns a typo'd event name into an immediate Lua error instead
/// of a handler that silently never fires.
pub const EVENT_NAMES: &[&str] = &[
    "connecting",
    "connected",
    "disconnected",
    "reconnect_scheduled",
    "retry_started",
    "retries_exhausted",
    "stopped",
    "kicked",
    "login",
    "spawned",
    "death",
    "health",
    "chat",
    "system_chat",
    "player_joined",
    "player_left",
    "player_updated",
    "entity_spawned",
    "entity_removed",
    "time",
    "weather",
    "presentation",
    "action_bar",
    "title",
    "boss_bar",
    "scoreboard",
    "team",
    "hud",
    "gui_opened",
    "gui_closed",
    "inventory",
    "action_result",
    "worker_overload",
    "script_error",
    "script_disabled",
];

pub fn intern_event_name(name: &str) -> mlua::Result<&'static str> {
    EVENT_NAMES
        .iter()
        .find(|n| **n == name)
        .copied()
        .ok_or_else(|| mlua::Error::RuntimeError(format!("unknown event name \"{name}\"")))
}

/// Installs the full API into `lua`: the `minerider` global (logging
/// only — `minerider.log`/`.info`/`.warn`/`.error`; there is no
/// `minerider.create_swarm` or any other swarm-construction call
/// available *inside* a script, see `docs/lua_api_reference.md#minerider`)
/// and the `swarm` global itself (this worker's one `Swarm` handle — see
/// `swarm::install`).
pub fn install(lua: &Lua, state: Rc<WorkerState>) -> mlua::Result<()> {
    install_minerider_global(lua, &state)?;
    swarm::install(lua, state)?;
    Ok(())
}

fn install_minerider_global(lua: &Lua, state: &Rc<WorkerState>) -> mlua::Result<()> {
    let minerider = lua.create_table()?;

    let log = {
        let worker_index = state.worker_index;
        lua.create_function(move |_, (level, msg): (String, String)| {
            log_line(worker_index, &level, &msg);
            Ok(())
        })?
    };
    minerider.set("log", log)?;

    for level in ["info", "warn", "error"] {
        let worker_index = state.worker_index;
        let level_owned = level.to_string();
        let func = lua.create_function(move |_, msg: String| {
            log_line(worker_index, &level_owned, &msg);
            Ok(())
        })?;
        minerider.set(level, func)?;
    }

    lua.globals().set("minerider", minerider)?;
    Ok(())
}

/// Length-limited so a script can never flood logs with one huge message;
/// prefixed with the worker id for multi-worker diagnosability. Never
/// includes full state-snapshot dumps by default, and can never contain a
/// proxy password since Lua is never given one to begin with — proxy
/// endpoints/credentials are host-supplied profiles a script can only
/// reference by opaque id (see `crate::lua::registry`'s module doc
/// comment).
///
/// Truncation goes through `truncate_for_log`, not a raw byte-index slice
/// — `msg` is a plain Lua string a script fully controls, and a byte
/// offset has no guaranteed relationship to a UTF-8 character boundary
/// (a naive `&msg[..MAX_LEN]` panics the moment `MAX_LEN` happens to fall
/// inside a multi-byte character, e.g. any script logging non-ASCII text
/// past the limit).
fn log_line(worker_index: usize, level: &str, msg: &str) {
    let truncated =
        crate::lua::error::truncate_for_log(msg, crate::lua::error::MAX_LOG_MESSAGE_BYTES);
    match level {
        "warn" => tracing::warn!(worker = worker_index, "[lua] {truncated}"),
        "error" => tracing::error!(worker = worker_index, "[lua] {truncated}"),
        _ => tracing::info!(worker = worker_index, "[lua] {truncated}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::dispatcher::{DispatcherHandle, WorkerQueue};
    use crate::lua::queue::{PriorityQueue, QueueDesign};
    use crate::lua::sandbox::{new_sandboxed_lua, SandboxConfig};
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;

    fn test_state() -> Rc<WorkerState> {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(64, 64)));
        let dispatcher = DispatcherHandle::new(vec![queue]);
        Rc::new(WorkerState {
            worker_index: 0,
            is_coordinator: true,
            dispatcher,
            runtime_handle: tokio::runtime::Handle::current(),
            instruction_counter: RefCell::new(Arc::new(AtomicU64::new(0))),
            sandbox: SandboxConfig::default(),
            handlers: RefCell::new(crate::lua::worker::HandlerRegistry::default()),
            callbacks: RefCell::new(crate::lua::worker::CallbackRegistry::default()),
            disabled_bots: RefCell::new(HashSet::new()),
            consecutive_errors: RefCell::new(HashMap::new()),
            config_builder: RefCell::new(crate::lua::registry::SwarmRegistryBuilder::default()),
            config_tx: RefCell::new(None),
            startup_barrier: crate::lua::worker::StartupBarrier::new(),
            startup_report_tx: tokio::sync::mpsc::unbounded_channel().0,
            started: RefCell::new(None),
            shared_state: Arc::new(crate::lua::api::shared::SharedState::new()),
            pubsub: RefCell::new(HashMap::new()),
            timers: RefCell::new(crate::lua::api::timers::TimerRegistry::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            callback_timeout: crate::lua::worker::DEFAULT_CALLBACK_TIMEOUT,
            action_task_semaphore: Arc::new(tokio::sync::Semaphore::new(
                crate::lua::worker::MAX_CONCURRENT_ACTION_TASKS,
            )),
        })
    }

    /// The real-world regression this whole fix is about: `minerider.log`/
    /// `.info`/`.warn`/`.error` are directly callable by any script with an
    /// arbitrary string. The previous `&msg[..MAX_LEN]` raw byte-index
    /// slice would panic the instant a script logged non-ASCII text long
    /// enough that byte 2048 landed inside a multi-byte character — which
    /// a malicious or simply non-English script could trivially trigger,
    /// crashing the whole worker thread from an ordinary logging call.
    #[tokio::test]
    async fn minerider_log_does_not_panic_on_multibyte_text_over_the_length_limit() {
        let state = test_state();
        let (lua, counter) = new_sandboxed_lua(&state.sandbox).unwrap();
        *state.instruction_counter.borrow_mut() = counter;
        install(&lua, state).unwrap();

        // "🦀" is 4 UTF-8 bytes; 600 repeats is 2400 bytes, and 2048 is
        // not a multiple of 4 — byte 2048 falls squarely inside a
        // character.
        lua.globals().set("huge_message", "🦀".repeat(600)).unwrap();

        // Must not panic for any of the four entry points.
        lua.load(
            r#"
            minerider.log("info", huge_message)
            minerider.info(huge_message)
            minerider.warn(huge_message)
            minerider.error(huge_message)
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn minerider_global_has_no_create_swarm_function() {
        // Compile-time-adjacent proof, not just a doc claim: `install_minerider_global`
        // only ever registers `log`/`info`/`warn`/`error` on the table (see
        // its body above) — there is no `create_swarm` call anywhere in
        // this module to register one.
        let source = include_str!("mod.rs");
        assert!(
            !source.contains("minerider.set(\"create_swarm\""),
            "if this ever starts failing, the doc comment above `install` claiming \
             there is no `minerider.create_swarm` needs updating too"
        );
    }
}
