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
///
/// Bounded by `WorkerState::action_task_semaphore`: a permit is acquired
/// *before* spawning (never inside the spawned task, which would just
/// move the same unbounded-spawn problem one step later) and held for
/// that action's whole lifetime. A script that issues actions faster than
/// they can complete — e.g. from a tight loop, or a `set_interval` timer
/// firing far more often than actions resolve — eventually exhausts the
/// semaphore; `try_acquire_owned` never blocks waiting for a permit, so
/// that action is instead resolved immediately with a typed
/// `worker_overloaded` error, entirely synchronously, spawning no task at
/// all.
fn spawn_action<F, Fut>(this: &LuaBot, op: F) -> u64
where
    F: FnOnce(SupervisorHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ActionOutcome> + Send + 'static,
{
    let request_id = this.state.dispatcher.allocate_request_id();
    let bot_id = this.bot_id;
    // The worker whose Lua VM is issuing this action — may differ from
    // `bot_id`'s owning worker when the script called `swarm:bot(id)` for
    // a bot it doesn't own. Threaded through so the eventual completion
    // can still reach a callback registered *here*, on this worker,
    // regardless of who owns the bot (see `DispatcherHandle::dispatch_action_result`).
    let origin_worker = this.state.worker_index;
    let dispatcher = this.state.dispatcher.clone();

    let permit = match this.state.action_task_semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            dispatcher.dispatch_action_result(
                origin_worker,
                ActionResult {
                    request_id,
                    bot_id,
                    outcome: ActionOutcome::Error(ScriptError::worker_overloaded()),
                },
            );
            return request_id;
        }
    };

    match this.state.bot_handle(bot_id) {
        Some(handle) => {
            this.state.runtime_handle.spawn(async move {
                let _permit = permit;
                let outcome = op(handle).await;
                dispatcher.dispatch_action_result(
                    origin_worker,
                    ActionResult {
                        request_id,
                        bot_id,
                        outcome,
                    },
                );
            });
        }
        None => {
            this.state.runtime_handle.spawn(async move {
                let _permit = permit;
                dispatcher.dispatch_action_result(
                    origin_worker,
                    ActionResult {
                        request_id,
                        bot_id,
                        outcome: ActionOutcome::Error(ScriptError::new(
                            "not_connected",
                            "this bot has no active connection handle yet",
                        )),
                    },
                );
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

/// Parses a `use_item_on_block` options table:
/// `{x, y, z, face, hand?, cursor_x?, cursor_y?, cursor_z?, inside_block?}`.
/// `x/y/z` and `face` are required; `hand` defaults to `"main"`, the cursor
/// to the face centre (0.5), `inside_block` to false. Range checks are left
/// to `BotCommand::validate`, but a coordinate that doesn't fit an `i32` is
/// rejected here.
fn parse_block_placement(t: &Table) -> mlua::Result<crate::minecraft::control::BlockPlacement> {
    let required_i32 = |key: &str| -> mlua::Result<i32> {
        let value: i64 = t.get::<Option<i64>>(key)?.ok_or_else(|| {
            mlua::Error::RuntimeError(format!("use_item_on_block requires integer field `{key}`"))
        })?;
        i32::try_from(value).map_err(|_| {
            mlua::Error::RuntimeError(format!(
                "use_item_on_block field `{key}` does not fit an i32"
            ))
        })
    };
    let face_i64: i64 = t.get::<Option<i64>>("face")?.ok_or_else(|| {
        mlua::Error::RuntimeError("use_item_on_block requires integer field `face` (0..=5)".into())
    })?;
    let hand = parse_hand(
        &t.get::<Option<String>>("hand")?
            .unwrap_or_else(|| "main".into()),
    )?;
    Ok(crate::minecraft::control::BlockPlacement {
        x: required_i32("x")?,
        y: required_i32("y")?,
        z: required_i32("z")?,
        face: i32::try_from(face_i64).unwrap_or(i32::MAX),
        hand,
        cursor_x: t.get::<Option<f32>>("cursor_x")?.unwrap_or(0.5),
        cursor_y: t.get::<Option<f32>>("cursor_y")?.unwrap_or(0.5),
        cursor_z: t.get::<Option<f32>>("cursor_z")?.unwrap_or(0.5),
        inside_block: t.get::<Option<bool>>("inside_block")?.unwrap_or(false),
    })
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
        // `bot_id % worker_count` — the bot's *owning* worker, which can
        // differ from `this.state.worker_index` (the worker whose Lua VM
        // happens to be executing this call): `swarm:bot(id)` may return a
        // bot owned by any worker, not just the caller's own.
        methods.add_method("worker_id", |_, this, ()| {
            Ok(this.state.dispatcher.worker_index_for(this.bot_id))
        });
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
        methods.add_method("select_hotbar_slot", |_, this, slot: i64| {
            // Reject out-of-range synchronously (like `parse_hand`), so a
            // caller bug surfaces at the call site rather than as an async
            // action error. 0..=8 are the nine hotbar slots.
            if !(0..=8).contains(&slot) {
                return Err(mlua::Error::RuntimeError(format!(
                    "select_hotbar_slot expects 0..=8, got {slot}"
                )));
            }
            let slot = slot as i16;
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.select_hotbar_slot(slot).await)
            }))
        });
        methods.add_method("use_item_on_block", |_, this, opts: Table| {
            let placement = parse_block_placement(&opts)?;
            Ok(spawn_action(this, move |h| async move {
                control_outcome(h.use_item_on_block(placement).await)
            }))
        });
        methods.add_method(
            "interact_entity",
            |_, this, (entity_id, opts): (i64, Option<Table>)| {
                let entity_id = i32::try_from(entity_id).map_err(|_| {
                    mlua::Error::RuntimeError(format!("entity_id {entity_id} does not fit an i32"))
                })?;
                let (hand, sneaking) = match &opts {
                    Some(t) => (
                        parse_hand(
                            &t.get::<Option<String>>("hand")?
                                .unwrap_or_else(|| "main".into()),
                        )?,
                        t.get::<Option<bool>>("sneaking")?.unwrap_or(false),
                    ),
                    None => (crate::minecraft::control::Hand::Main, false),
                };
                Ok(spawn_action(this, move |h| async move {
                    control_outcome(h.interact_entity(entity_id, hand, sneaking).await)
                }))
            },
        );
        methods.add_method("release_item", |_, this, ()| {
            Ok(spawn_action(this, |h| async move {
                control_outcome(h.release_item().await)
            }))
        });
        methods.add_method("close_gui", |_, this, ()| {
            Ok(spawn_action(this, |h| async move {
                control_outcome(h.close_gui().await)
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
                crate::lua::api::timers::schedule_for_bot(
                    &this.state,
                    this.bot_id,
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
                crate::lua::api::timers::schedule_for_bot(
                    &this.state,
                    this.bot_id,
                    lua,
                    func,
                    Duration::from_millis(interval_ms),
                    Some(Duration::from_millis(interval_ms)),
                )
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
            },
        );
        methods.add_method("clear_timer", |lua, this, id: u64| {
            Ok(crate::lua::api::timers::clear(&this.state, lua, id))
        });

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
            Ok(this.state.remove_handler_or_subscription(id))
        });
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

    fn test_state(action_task_capacity: usize) -> Rc<WorkerState> {
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
            action_task_semaphore: Arc::new(tokio::sync::Semaphore::new(action_task_capacity)),
        })
    }

    /// The core Fix 6 regression: an exhausted action-task semaphore must
    /// resolve the action immediately (entirely synchronously, no task
    /// spawned) with a typed `worker_overloaded` error, never block the
    /// calling Lua thread waiting for a permit and never spawn an
    /// unbounded number of tasks past the configured cap.
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_action_resolves_immediately_with_worker_overloaded_when_the_semaphore_is_exhausted(
    ) {
        let state = test_state(0); // zero permits: every action must be rejected
        let (_lua, counter) = new_sandboxed_lua(&state.sandbox).unwrap();
        *state.instruction_counter.borrow_mut() = counter;
        let bot = LuaBot {
            state: state.clone(),
            bot_id: 0,
        };

        let request_id = spawn_action(&bot, |_handle| async { ActionOutcome::DeliveredSent });

        // Resolved synchronously: the result must already be sitting in
        // this worker's own queue, with no `.await`/task-yield needed.
        let queue = state.dispatcher.queue(0);
        let batch = queue.wait_for_batch();
        assert_eq!(batch.len(), 1);
        match &batch[0].event {
            crate::lua::event::WorkItem::ActionResult(result) => {
                assert_eq!(result.request_id, request_id);
                match &result.outcome {
                    ActionOutcome::Error(e) => assert_eq!(e.code, "worker_overloaded"),
                    other => panic!("expected Error(worker_overloaded), got {other:?}"),
                }
            }
            other => panic!("expected ActionResult, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_action_succeeds_normally_when_the_semaphore_has_capacity() {
        let state = test_state(4);
        let (_lua, counter) = new_sandboxed_lua(&state.sandbox).unwrap();
        *state.instruction_counter.borrow_mut() = counter;
        let bot = LuaBot {
            state: state.clone(),
            bot_id: 0,
        };

        // No bot handle registered — resolves as `not_connected`, but the
        // point here is that it goes through the normal spawn path (a
        // permit was available), not the semaphore-exhausted short
        // circuit.
        let request_id = spawn_action(&bot, |_handle| async { ActionOutcome::DeliveredSent });

        let queue = state.dispatcher.queue(0);
        let batch = queue.wait_for_batch();
        assert_eq!(batch.len(), 1);
        match &batch[0].event {
            crate::lua::event::WorkItem::ActionResult(result) => {
                assert_eq!(result.request_id, request_id);
                match &result.outcome {
                    ActionOutcome::Error(e) => assert_eq!(e.code, "not_connected"),
                    other => panic!("expected Error(not_connected), got {other:?}"),
                }
            }
            other => panic!("expected ActionResult, got {other:?}"),
        }
    }
}
