//! Registers the small benchmark-only Lua API (`swarm:on`, `bot:command`)
//! into a sandboxed [`mlua::Lua`], and converts a [`BenchEvent`] into the
//! Lua event table a handler receives — mirroring the illustrative API in
//! the mission and `docs/lua_design.md`'s proposed `bot:*` surface, kept
//! deliberately small.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use mlua::{Lua, Table, Value, Variadic};

use super::command::{BenchCommand, CommandSink};
use super::event::{BenchEvent, BotId};
use super::metrics::LatencySamples;

/// What handler-triggered `bot.command` calls need to know about the event
/// currently being dispatched: its name (for command source-attribution)
/// and its original enqueue time (for enqueue-to-command-emission latency).
/// Written once per handler call by the dispatch loop, read at most a
/// handful of times per call by `bot.command`; never contended since one
/// worker thread owns both the VM and this context.
#[derive(Clone, Copy)]
pub struct DispatchContext {
    pub event_name: &'static str,
    pub envelope_enqueued_at: Instant,
}

pub type SharedDispatchContext = Arc<Mutex<DispatchContext>>;

/// Bootstrap prepended to every script: defines `swarm.on(name, fn)` (Lua
/// table-of-handler-lists) so `_HANDLERS` stays pure Lua state and the Rust
/// side only ever reaches in to read `_HANDLERS[name]`, never to reimplement
/// registration itself.
const BOOTSTRAP: &str = r#"
_HANDLERS = {}
swarm = {}
function swarm.on(name, fn)
    if _HANDLERS[name] == nil then
        _HANDLERS[name] = {}
    end
    table.insert(_HANDLERS[name], fn)
end
"#;

/// Registers `bot.command(bot_id, action, ...)` as a real Rust callback
/// writing into `sink`, then loads the bootstrap and `script_body` (in that
/// order, one chunk) into `lua`. Call once per VM at worker/bot startup —
/// never per event, matching "do not create a VM per event".
pub fn install(
    lua: &Lua,
    sink: CommandSink,
    context: SharedDispatchContext,
    command_latency: Arc<Mutex<LatencySamples>>,
    script_body: &str,
) -> mlua::Result<()> {
    let bot_table = lua.create_table()?;
    bot_table.set(
        "command",
        lua.create_function(move |_, args: Variadic<Value>| {
            let mut iter = args.into_iter();
            let bot_id = match iter.next() {
                Some(Value::Integer(i)) => BotId(i as u32),
                Some(Value::Number(n)) => BotId(n as u32),
                _ => return Err(mlua::Error::RuntimeError("bot.command: missing bot_id".into())),
            };
            let action = match iter.next() {
                Some(Value::String(s)) => s.to_str()?.to_string(),
                _ => return Err(mlua::Error::RuntimeError("bot.command: missing action".into())),
            };
            let command = match action.as_str() {
                "forward" => {
                    let on = matches!(iter.next(), Some(Value::Boolean(true)));
                    BenchCommand::Forward(on)
                }
                "chat" => {
                    let message = match iter.next() {
                        Some(Value::String(s)) => s.to_str()?.to_string(),
                        _ => String::new(),
                    };
                    BenchCommand::Chat(message)
                }
                "click_slot" => {
                    let raw_slot = match iter.next() {
                        Some(Value::Integer(i)) => i as u16,
                        Some(Value::Number(n)) => n as u16,
                        _ => 0,
                    };
                    let right_click = matches!(iter.next(), Some(Value::String(s)) if s.to_str().map(|s| s == "right").unwrap_or(false));
                    BenchCommand::ClickSlot { raw_slot, right_click }
                }
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "bot.command: unknown action {other:?}"
                    )))
                }
            };
            let ctx = *context.lock().unwrap();
            sink.emit(bot_id, command, ctx.event_name);
            command_latency
                .lock()
                .unwrap()
                .record(ctx.envelope_enqueued_at.elapsed());
            Ok(())
        })?,
    )?;
    lua.globals().set("bot", bot_table)?;

    lua.load(BOOTSTRAP).exec()?;
    lua.load(script_body).exec()?;
    Ok(())
}

/// Looks up every handler registered for `event_name` via `swarm.on`, in
/// registration order.
pub fn handlers_for(lua: &Lua, event_name: &str) -> mlua::Result<Vec<mlua::Function>> {
    let handlers_table: Table = lua.globals().get("_HANDLERS")?;
    let list: Option<Table> = handlers_table.get(event_name)?;
    let Some(list) = list else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pair in list.sequence_values::<mlua::Function>() {
        out.push(pair?);
    }
    Ok(out)
}

/// Converts one [`BenchEvent`] into the plain Lua table a handler receives
/// as its second argument — never a userdata wrapping Rust internals, per
/// `docs/lua_design.md`'s "no raw Rust internals are ever handed to Lua".
pub fn event_to_table(lua: &Lua, event: &BenchEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        BenchEvent::Connected => {}
        BenchEvent::Disconnected { reason_len } => t.set("reason_len", *reason_len)?,
        BenchEvent::ReconnectScheduled { attempt, delay_ms } => {
            t.set("attempt", *attempt)?;
            t.set("delay_ms", *delay_ms)?;
        }
        BenchEvent::Health { health, food } => {
            t.set("health", *health)?;
            t.set("food", *food)?;
        }
        BenchEvent::Chat {
            sender_len,
            message,
        } => {
            t.set("sender_len", *sender_len)?;
            t.set("message", message.as_str())?;
        }
        BenchEvent::PlayerJoined { name_len } => t.set("name_len", *name_len)?,
        BenchEvent::InventorySlotUpdate {
            slot,
            item_id,
            count,
        } => {
            t.set("slot", *slot)?;
            t.set("item_id", *item_id)?;
            t.set("count", *count)?;
        }
        BenchEvent::GuiOpened { window_id, slots } => {
            t.set("window_id", *window_id)?;
            let slots_table = lua.create_table()?;
            for (i, slot) in slots.iter().enumerate() {
                let s = lua.create_table()?;
                s.set("raw_slot", slot.raw_slot)?;
                s.set("item_id", slot.item_id)?;
                s.set("count", slot.count)?;
                slots_table.set(i + 1, s)?;
            }
            t.set("slots", slots_table)?;
        }
        BenchEvent::StateSummary {
            entity_count,
            hud_effects,
            selected_entity_id,
        } => {
            t.set("entity_count", *entity_count)?;
            t.set("hud_effects", *hud_effects)?;
            if let Some(id) = selected_entity_id {
                t.set("selected_entity_id", *id)?;
            }
        }
        BenchEvent::EntityTick { entity_id, x, y, z } => {
            t.set("entity_id", *entity_id)?;
            t.set("x", *x)?;
            t.set("y", *y)?;
            t.set("z", *z)?;
        }
    }
    Ok(t)
}

#[cfg(test)]
pub(crate) fn test_context(
    event_name: &'static str,
) -> (SharedDispatchContext, Arc<Mutex<LatencySamples>>) {
    (
        Arc::new(Mutex::new(DispatchContext {
            event_name,
            envelope_enqueued_at: Instant::now(),
        })),
        Arc::new(Mutex::new(LatencySamples::with_capacity(64))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::sandbox::{new_sandboxed_lua, SandboxConfig};

    #[test]
    fn swarm_on_registers_a_handler_invoked_on_dispatch() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let (sink, _rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("connected");
        install(
            &lua,
            sink,
            ctx,
            lat,
            r#"
            swarm.on("connected", function(bot_id, event)
                LAST_BOT_ID = bot_id
            end)
            "#,
        )
        .unwrap();
        let handlers = handlers_for(&lua, "connected").unwrap();
        assert_eq!(handlers.len(), 1);
        let table = event_to_table(&lua, &BenchEvent::Connected).unwrap();
        handlers[0].call::<()>((BotId(7).0, table)).unwrap();
        let last: u32 = lua.globals().get("LAST_BOT_ID").unwrap();
        assert_eq!(last, 7);
    }

    #[test]
    fn multiple_handlers_for_the_same_event_all_run_in_registration_order() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let (sink, _rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("connected");
        install(
            &lua,
            sink,
            ctx,
            lat,
            r#"
            ORDER = {}
            swarm.on("connected", function() table.insert(ORDER, 1) end)
            swarm.on("connected", function() table.insert(ORDER, 2) end)
            "#,
        )
        .unwrap();
        let table = event_to_table(&lua, &BenchEvent::Connected).unwrap();
        for handler in handlers_for(&lua, "connected").unwrap() {
            handler.call::<()>((1u32, table.clone())).unwrap();
        }
        let order: Vec<i64> = lua
            .globals()
            .get::<Table>("ORDER")
            .unwrap()
            .sequence_values()
            .collect::<mlua::Result<_>>()
            .unwrap();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn bot_command_reaches_the_sink_with_correct_bot_id_and_action() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let (sink, rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("chat");
        install(
            &lua,
            sink,
            ctx,
            lat,
            r#"
            swarm.on("chat", function(bot_id, event)
                if event.message == "move" then
                    bot.command(bot_id, "forward", true)
                end
            end)
            "#,
        )
        .unwrap();
        let event = BenchEvent::Chat {
            sender_len: 3,
            message: "move".to_string(),
        };
        let table = event_to_table(&lua, &event).unwrap();
        for handler in handlers_for(&lua, "chat").unwrap() {
            handler.call::<()>((BotId(3).0, table.clone())).unwrap();
        }
        let record = rx.try_recv().unwrap();
        assert_eq!(record.bot_id, BotId(3));
        assert_eq!(record.command, BenchCommand::Forward(true));
        assert_eq!(record.source_event, "chat");
    }

    #[test]
    fn gui_opened_event_exposes_a_bounded_slots_array() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let event = BenchEvent::synthetic(super::super::event::SizeClass::Large, BotId(0), 0);
        let table = event_to_table(&lua, &event).unwrap();
        let slots: Table = table.get("slots").unwrap();
        assert_eq!(slots.raw_len(), super::super::event::GUI_OPENED_MAX_SLOTS);
    }
}
