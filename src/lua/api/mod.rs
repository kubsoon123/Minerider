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

/// Installs the full API into `lua`: the `minerider` global (logging,
/// `create_swarm`) and the `swarm` global itself (this worker's one
/// `Swarm` handle — see `swarm::install`).
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

/// Length-limited (2 KiB) so a script can never flood logs with one huge
/// message; prefixed with the worker id for multi-worker diagnosability.
/// Never includes full state-snapshot dumps by default, and can never
/// contain a proxy password since Lua is never given one to begin with
/// (see `crate::lua::registry::ProxyDef::resolve`).
fn log_line(worker_index: usize, level: &str, msg: &str) {
    const MAX_LEN: usize = 2048;
    let truncated = if msg.len() > MAX_LEN {
        format!("{}... [truncated]", &msg[..MAX_LEN])
    } else {
        msg.to_string()
    };
    match level {
        "warn" => tracing::warn!(worker = worker_index, "[lua] {truncated}"),
        "error" => tracing::error!(worker = worker_index, "[lua] {truncated}"),
        _ => tracing::info!(worker = worker_index, "[lua] {truncated}"),
    }
}
