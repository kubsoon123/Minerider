//! `LuaBot`: the `bot` userdata every event handler receives and
//! `swarm:bot(id)`/`swarm:bots()` return.
//!
//! Read-only queries (`state`, `player`, `entities`, `players`,
//! `inventory`, `hud`, `presentation`, `scoreboard`, `open_gui`, `status`,
//! `generation`) are synchronous — they read an already-published `watch`
//! snapshot, never block on the network. Every action that reaches the
//! network (movement, look, chat, hand actions, GUI/inventory clicks)
//! follows the mission's non-blocking model: validate → allocate a request
//! id → spawn the actual async call on the shared tokio runtime → return
//! the request id immediately. The result arrives later via the worker's
//! `action_result` event and (if one was given) a one-shot callback — see
//! `spawn_action` below and `crate::lua::worker::dispatch_one`.

use std::rc::Rc;
use std::time::Duration;

use mlua::{Lua, Table, UserData, UserDataMethods, Value};

use crate::core::supervisor::{ControlError, SupervisorHandle, SupervisorStatus};
use crate::lua::dispatcher::{ActionOutcome, ActionResult};
use crate::lua::error::ScriptError;
use crate::lua::worker::WorkerState;
use crate::minecraft::control::{Hand, RandomLookConfig};
use crate::minecraft::inventory::{DragButton, GuiClick, InventoryOutcome};
use crate::minecraft::player::MovementInput;

#[derive(Clone)]
pub struct LuaBot {
    pub state: Rc<WorkerState>,
    pub bot_id: u32,
}

pub fn make_bot(lua: &Lua, state: Rc<WorkerState>, bot_id: u32) -> mlua::Result<Value> {
    lua.pack(LuaBot { state, bot_id })
}

fn status_to_str(status: SupervisorStatus) -> String {
    match status {
        SupervisorStatus::Disconnected => "disconnected".to_string(),
        SupervisorStatus::Connecting => "connecting".to_string(),
        SupervisorStatus::Connected => "connected".to_string(),
        SupervisorStatus::ReconnectScheduled { attempt } => {
            format!("reconnect_scheduled:{attempt}")
        }
        SupervisorStatus::Stopped => "stopped".to_string(),
    }
}

/// Spawns `op(handle)` on the shared tokio runtime and routes its outcome
/// back into this bot's worker as a high-priority `action_result` event
/// (see `crate::lua::dispatcher::DispatcherHandle::dispatch_action_result`).
/// Never blocks the calling Lua thread.
fn spawn_action<F, Fut>(this: &LuaBot, op: F) -> u64
where
    F: FnOnce(SupervisorHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ActionOutcome> + Send + 'static,
{
    let request_id = this.state.dispatcher.allocate_request_id();
    let bot_id = this.bot_id;
    let dispatcher = this.state.dispatcher.clone();
    match this.state.bot_handle(bot_id) {
        Some(handle) => {
            this.state.runtime_handle.spawn(async move {
                let outcome = op(handle).await;
                dispatcher.dispatch_action_result(ActionResult {
                    request_id,
                    bot_id,
                    outcome,
                });
            });
        }
        None => {
            this.state.runtime_handle.spawn(async move {
                dispatcher.dispatch_action_result(ActionResult {
                    request_id,
                    bot_id,
                    outcome: ActionOutcome::Error(ScriptError::new(
                        "not_connected",
                        "this bot has no active connection handle yet",
                    )),
                });
            });
        }
    }
    request_id
}

fn control_outcome(result: Result<(), ControlError>) -> ActionOutcome {
    match result {
        Ok(()) => ActionOutcome::DeliveredSent,
        Err(e) => ActionOutcome::Error(e.into()),
    }
}

fn gui_outcome(
    result: Result<InventoryOutcome, crate::core::supervisor::GuiActionError>,
) -> ActionOutcome {
    match result {
        Ok(InventoryOutcome::Sent) => ActionOutcome::DeliveredSent,
        Ok(InventoryOutcome::Confirmed { state_id }) => ActionOutcome::Confirmed { state_id },
        Ok(InventoryOutcome::Corrected { state_id }) => ActionOutcome::Corrected { state_id },
        Ok(InventoryOutcome::TimedOut) => ActionOutcome::Error(
            ScriptError::new("inventory_timeout", "inventory transaction timed out")
                .retryable(true),
        ),
        Ok(InventoryOutcome::WindowClosed) => ActionOutcome::Error(ScriptError::new(
            "no_gui_open",
            "the window closed before the transaction completed",
        )),
        Ok(InventoryOutcome::Rejected(e)) => ActionOutcome::Error(e.into()),
        Err(e) => ActionOutcome::Error(e.into()),
    }
}

fn parse_hand(s: &str) -> mlua::Result<Hand> {
    match s {
        "main" => Ok(Hand::Main),
        "off" => Ok(Hand::Off),
        other => Err(mlua::Error::RuntimeError(format!(
            "invalid hand \"{other}\" (expected \"main\" or \"off\")"
        ))),
    }
}

fn parse_drag_button(s: &str) -> mlua::Result<DragButton> {
    match s {
        "left" => Ok(DragButton::Left),
        "right" => Ok(DragButton::Right),
        "middle" => Ok(DragButton::Middle),
        other => Err(mlua::Error::RuntimeError(format!(
            "invalid drag button \"{other}\" (expected \"left\"/\"right\"/\"middle\")"
        ))),
    }
}

/// Parses one of the 15 `GuiClick` modes, e.g. `"left"`, `"hotbar_swap:3"`,
/// `"drag_start:left"`. See `docs/lua_api_reference.md#gui-click-modes`.
pub fn parse_gui_click(mode: &str) -> mlua::Result<GuiClick> {
    let (kind, param) = match mode.split_once(':') {
        Some((k, p)) => (k, Some(p)),
        None => (mode, None),
    };
    fn need_param<'a>(mode: &str, p: Option<&'a str>) -> mlua::Result<&'a str> {
        p.ok_or_else(|| {
            mlua::Error::RuntimeError(format!("mode \"{mode}\" requires a \":param\" suffix"))
        })
    }
    Ok(match kind {
        "left" => GuiClick::Left,
        "right" => GuiClick::Right,
        "shift_left" => GuiClick::ShiftLeft,
        "shift_right" => GuiClick::ShiftRight,
        "hotbar_swap" => {
            let n: u8 = need_param(mode, param)?.parse().map_err(|_| {
                mlua::Error::RuntimeError("hotbar_swap param must be 0..=8".to_string())
            })?;
            GuiClick::HotbarSwap(n)
        }
        "offhand_swap" => GuiClick::OffhandSwap,
        "throw_one" => GuiClick::ThrowOne,
        "throw_stack" => GuiClick::ThrowStack,
        "double_click" => GuiClick::DoubleClick,
        "outside_left" => GuiClick::OutsideLeft,
        "outside_right" => GuiClick::OutsideRight,
        "drag_start" => GuiClick::DragStart(parse_drag_button(need_param(mode, param)?)?),
        "drag_add_slot" => GuiClick::DragAddSlot(parse_drag_button(need_param(mode, param)?)?),
        "drag_end" => GuiClick::DragEnd(parse_drag_button(need_param(mode, param)?)?),
        "creative_clone" => GuiClick::CreativeClone,
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "unknown gui click mode \"{other}\""
            )))
        }
    })
}

/// Registers a one-shot callback for `request_id` if `callback` is a
/// function (ignored if `nil`), bounded per
/// `crate::lua::worker::MAX_PENDING_CALLBACKS` (oldest evicted on
/// overflow — consistent with this wrapper's overflow-drops-oldest
/// philosophy elsewhere).
fn register_callback(
    lua: &Lua,
    state: &Rc<WorkerState>,
    bot_id: u32,
    request_id: u64,
    callback: Value,
) -> mlua::Result<()> {
    if let Value::Function(f) = callback {
        let key = lua.create_registry_value(f)?;
        let mut callbacks = state.callbacks.borrow_mut();
        if callbacks.pending.len() >= crate::lua::worker::MAX_PENDING_CALLBACKS {
            if let Some(oldest_id) = callbacks
                .pending
                .iter()
                .min_by_key(|(_, cb)| cb.registered_at)
                .map(|(id, _)| *id)
            {
                if let Some(evicted) = callbacks.pending.remove(&oldest_id) {
                    let _ = lua.remove_registry_value(evicted.key);
                }
            }
        }
        callbacks.pending.insert(
            request_id,
            crate::lua::worker::PendingCallback {
                key,
                bot_id,
                registered_at: std::time::Instant::now(),
            },
        );
    }
    Ok(())
}

fn parse_random_look(value: Value) -> mlua::Result<Option<RandomLookConfig>> {
    match value {
        Value::Nil => Ok(None),
        Value::Table(t) => {
            let mut cfg = RandomLookConfig {
                min_interval: Duration::from_millis(500),
                max_interval: Duration::from_millis(2000),
                max_yaw_delta: 45.0,
                min_pitch: -20.0,
                max_pitch: 20.0,
                seed: None,
            };
            if let Ok(Value::Integer(v)) = t.get::<Value>("min_interval_ms") {
                cfg.min_interval = Duration::from_millis(v.max(0) as u64);
            }
            if let Ok(Value::Integer(v)) = t.get::<Value>("max_interval_ms") {
                cfg.max_interval = Duration::from_millis(v.max(0) as u64);
            }
            if let Ok(v) = t.get::<f32>("max_yaw_delta") {
                cfg.max_yaw_delta = v;
            }
            if let Ok(v) = t.get::<f32>("min_pitch") {
                cfg.min_pitch = v;
            }
            if let Ok(v) = t.get::<f32>("max_pitch") {
                cfg.max_pitch = v;
            }
            if let Ok(Value::Integer(v)) = t.get::<Value>("seed") {
                cfg.seed = Some(v as u64);
            }
            Ok(Some(cfg))
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "set_random_look expects a table or nil, got {}",
            other.type_name()
        ))),
    }
}

macro_rules! bool_action {
    ($methods:ident, $name:literal, $call:ident) => {
        $methods.add_method($name, |_, this, on: bool| {
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.$call(on).await)
            }))
        });
    };
}

impl UserData for LuaBot {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ---- Identity / metadata ------------------------------------
        methods.add_method("id", |_, this, ()| Ok(this.bot_id));
        methods.add_method("worker_id", |_, this, ()| Ok(this.state.worker_index));
        methods.add_method("username", |_, this, ()| {
            Ok(this
                .state
                .registry()
                .and_then(|r| r.bots.get(&this.bot_id).map(|b| b.username.clone())))
        });
        methods.add_method("server", |_, this, ()| {
            Ok(this
                .state
                .registry()
                .and_then(|r| r.bots.get(&this.bot_id).map(|b| b.server.clone())))
        });
        methods.add_method("proxy", |_, this, ()| {
            Ok(this
                .state
                .registry()
                .and_then(|r| r.bots.get(&this.bot_id).and_then(|b| b.proxy.clone())))
        });
        methods.add_method("groups", |lua, this, ()| {
            let names: Vec<String> = this
                .state
                .registry()
                .map(|r| {
                    r.groups
                        .values()
                        .filter(|g| g.bot_ids.contains(&this.bot_id))
                        .map(|g| g.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            crate::lua::convert::string_array(lua, names)
        });
        methods.add_method("status", |_, this, ()| {
            Ok(this
                .state
                .bot_handle(this.bot_id)
                .map(|h| status_to_str(*h.status().borrow())))
        });
        methods.add_method("generation", |_, this, ()| {
            Ok(this.state.bot_handle(this.bot_id).map(|h| h.generation()))
        });

        // ---- Lifecycle -------------------------------------------------
        methods.add_method("disconnect", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => {
                    handle.stop();
                    Ok((Value::Boolean(true), Value::Nil))
                }
                None => crate::lua::api::errors::err_pair(
                    lua,
                    ScriptError::new("not_connected", "this bot has no active connection handle"),
                ),
            }
        });
        methods.add_method("stop", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => {
                    handle.stop();
                    Ok((Value::Boolean(true), Value::Nil))
                }
                None => crate::lua::api::errors::err_pair(
                    lua,
                    ScriptError::new("not_connected", "this bot has no active connection handle"),
                ),
            }
        });
        methods.add_method("connect", |lua, _this, ()| {
            // Bots connect automatically once the swarm starts, and the
            // supervisor's own reconnect policy governs every subsequent
            // attempt; there is no manual (re)connect trigger for an
            // already-stopped supervisor in this version (see
            // `docs/lua_api_reference.md#limitations`).
            crate::lua::api::errors::err_pair(
                lua,
                ScriptError::new(
                    "invalid_action",
                    "bots connect automatically at swarm startup; manual (re)connect of a stopped bot is not supported",
                ),
            )
        });

        // ---- Movement ----------------------------------------------
        bool_action!(methods, "forward", forward);
        bool_action!(methods, "backward", backward);
        bool_action!(methods, "strafe_left", strafe_left);
        bool_action!(methods, "strafe_right", strafe_right);
        bool_action!(methods, "jump", jump);
        bool_action!(methods, "sneak", sneak);
        bool_action!(methods, "sprint", sprint);

        methods.add_method("set_input", |_, this, table: Table| {
            let input = MovementInput {
                forward: table.get::<f32>("forward").unwrap_or(0.0),
                strafe: table.get::<f32>("strafe").unwrap_or(0.0),
                jump: table.get::<bool>("jump").unwrap_or(false),
                sprint: table.get::<bool>("sprint").unwrap_or(false),
                sneak: table.get::<bool>("sneak").unwrap_or(false),
            };
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.set_input(input).await)
            }))
        });
        methods.add_method("stop_movement", |_, this, ()| {
            Ok(spawn_action(this, |h| async move {
                control_outcome(h.stop_movement().await)
            }))
        });
        methods.add_method("walk_to", |_, this, (x, z): (f64, f64)| {
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.walk_to(x, z).await)
            }))
        });

        // ---- Looking -------------------------------------------------
        methods.add_method("look", |_, this, (yaw, pitch): (f32, f32)| {
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.look(yaw, pitch).await)
            }))
        });
        methods.add_method("set_random_look", |_, this, value: Value| {
            let cfg = parse_random_look(value)?;
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.set_random_look(cfg).await)
            }))
        });

        // ---- Chat / command (kept separate; no `/`-inference) --------
        methods.add_method("chat", |_, this, message: String| {
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.chat(message).await)
            }))
        });
        methods.add_method("command", |_, this, command: String| {
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.command(command).await)
            }))
        });

        // ---- Hand actions --------------------------------------------
        methods.add_method("use_item", |_, this, hand: String| {
            let hand = parse_hand(&hand)?;
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.use_item(hand).await)
            }))
        });
        methods.add_method("swing", |_, this, hand: String| {
            let hand = parse_hand(&hand)?;
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.swing(hand).await)
            }))
        });

        // ---- State (synchronous, read-only, detached) -----------------
        methods.add_method("state", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::state::state_snapshot_to_table(
                    lua,
                    &handle.state().borrow(),
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("player", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::state::player_to_table(
                    lua,
                    &handle.state().borrow().player,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("entities", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::state::entities_to_table(
                    lua,
                    &handle.state().borrow().entities,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("players", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::state::players_to_table(
                    lua,
                    &handle.state().borrow().players,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("inventory", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::state::inventory_state_to_table(
                    lua,
                    &handle.state().borrow().inventory,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("hud", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => {
                    crate::lua::convert::hud::hud_state_to_table(lua, &handle.state().borrow().hud)
                        .map(Value::Table)
                }
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("presentation", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::presentation::presentation_state_to_table(
                    lua,
                    &handle.state().borrow().presentation,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("scoreboard", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => crate::lua::convert::scoreboard::scoreboard_state_to_table(
                    lua,
                    &handle.state().borrow().scoreboard,
                )
                .map(Value::Table),
                None => Ok(Value::Nil),
            }
        });

        // ---- GUI inspection / clicking --------------------------------
        methods.add_method("open_gui", |lua, this, ()| {
            match this.state.bot_handle(this.bot_id) {
                Some(handle) => {
                    crate::lua::convert::gui::opt_gui_view_to_value(lua, handle.open_gui().as_ref())
                }
                None => Ok(Value::Nil),
            }
        });
        methods.add_method(
            "click_gui",
            |lua, this, (slot, mode, callback): (usize, String, Value)| {
                let click = parse_gui_click(&mode)?;
                let request_id = spawn_action(this, move |h| async move {
                    gui_outcome(h.click_open_gui_slot(slot, click).await)
                });
                register_callback(lua, &this.state, this.bot_id, request_id, callback)?;
                Ok(request_id)
            },
        );
        methods.add_method(
            "click_inventory",
            |lua, this, (slot, mode, callback): (usize, String, Value)| {
                let click = parse_gui_click(&mode)?;
                let request_id = spawn_action(this, move |h| async move {
                    gui_outcome(h.click_inventory_slot(slot, click).await)
                });
                register_callback(lua, &this.state, this.bot_id, request_id, callback)?;
                Ok(request_id)
            },
        );

        // ---- Timers (bot-scoped, run on this bot's own worker) --------
        methods.add_method(
            "set_timeout",
            |lua, this, (delay_ms, func): (u64, mlua::Function)| {
                crate::lua::api::timers::schedule(
                    &this.state,
                    lua,
                    func,
                    Duration::from_millis(delay_ms),
                    None,
                )
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
            },
        );
        methods.add_method(
            "set_interval",
            |lua, this, (interval_ms, func): (u64, mlua::Function)| {
                crate::lua::api::timers::schedule(
                    &this.state,
                    lua,
                    func,
                    Duration::from_millis(interval_ms),
                    Some(Duration::from_millis(interval_ms)),
                )
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
            },
        );

        // ---- Events ----------------------------------------------------
        methods.add_method("on", |lua, this, (name, func): (String, mlua::Function)| {
            let name = crate::lua::api::intern_event_name(&name)?;
            let key = lua.create_registry_value(func)?;
            Ok(this
                .state
                .handlers
                .borrow_mut()
                .register_bot(this.bot_id, name, key, false))
        });
        methods.add_method(
            "once",
            |lua, this, (name, func): (String, mlua::Function)| {
                let name = crate::lua::api::intern_event_name(&name)?;
                let key = lua.create_registry_value(func)?;
                Ok(this
                    .state
                    .handlers
                    .borrow_mut()
                    .register_bot(this.bot_id, name, key, true))
            },
        );
        methods.add_method("off", |_, this, id: u64| {
            Ok(this.state.handlers.borrow_mut().remove(id))
        });
    }
}
