//! Exhaustive `BotEvent` → Lua table conversion, and `ActionResult` →
//! table. Every match is written without a wildcard arm, mirroring
//! `crate::lua::event`'s exhaustive priority classification — a new
//! `BotEvent` variant must force a compile error here too.

use mlua::{Lua, Table, Value};

use crate::lua::dispatcher::{ActionOutcome, ActionResult};
use crate::minecraft::event::BotEvent;
use crate::minecraft::hud::HudEvent;
use crate::minecraft::inventory::InventoryEvent;

fn inventory_event_to_table(lua: &Lua, event: &InventoryEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        InventoryEvent::WindowOpened { window_id } => {
            t.set("kind", "window_opened")?;
            t.set("window_id", *window_id)?;
        }
        InventoryEvent::WindowClosed { window_id } => {
            t.set("kind", "window_closed")?;
            t.set("window_id", *window_id)?;
        }
        InventoryEvent::WindowSynchronized {
            window_id,
            state_id,
        } => {
            t.set("kind", "window_synchronized")?;
            t.set("window_id", *window_id)?;
            t.set("state_id", *state_id)?;
        }
        InventoryEvent::SlotUpdated {
            window_id,
            state_id,
            slot,
        } => {
            t.set("kind", "slot_updated")?;
            t.set("window_id", *window_id)?;
            t.set("state_id", *state_id)?;
            t.set("slot", *slot)?;
        }
        InventoryEvent::CursorUpdated => {
            t.set("kind", "cursor_updated")?;
        }
        InventoryEvent::PropertyUpdated {
            window_id,
            property,
        } => {
            t.set("kind", "property_updated")?;
            t.set("window_id", *window_id)?;
            t.set("property", *property)?;
        }
        InventoryEvent::SelectedHotbarChanged { slot, applied } => {
            t.set("kind", "selected_hotbar_changed")?;
            t.set("slot", *slot)?;
            t.set("applied", *applied)?;
        }
        InventoryEvent::TransactionQueued { transaction_id } => {
            t.set("kind", "transaction_queued")?;
            t.set("transaction_id", *transaction_id)?;
        }
        InventoryEvent::TransactionSent { transaction_id } => {
            t.set("kind", "transaction_sent")?;
            t.set("transaction_id", *transaction_id)?;
        }
        InventoryEvent::TransactionFinished {
            transaction_id,
            outcome,
        } => {
            t.set("kind", "transaction_finished")?;
            t.set("transaction_id", *transaction_id)?;
            t.set("outcome", inventory_outcome_to_str(outcome))?;
        }
        InventoryEvent::TransactionRejected {
            transaction_id,
            error,
        } => {
            t.set("kind", "transaction_rejected")?;
            t.set("transaction_id", *transaction_id)?;
            t.set("error", error.to_string())?;
        }
    }
    Ok(t)
}

fn inventory_outcome_to_str(
    outcome: &crate::minecraft::inventory::InventoryOutcome,
) -> &'static str {
    use crate::minecraft::inventory::InventoryOutcome;
    match outcome {
        InventoryOutcome::Sent => "sent",
        InventoryOutcome::Confirmed { .. } => "confirmed",
        InventoryOutcome::Corrected { .. } => "corrected",
        InventoryOutcome::TimedOut => "timed_out",
        InventoryOutcome::WindowClosed => "window_closed",
        InventoryOutcome::Rejected(_) => "rejected",
    }
}

fn hud_event_to_table(lua: &Lua, event: &HudEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        HudEvent::VitalsChanged { vitals } => {
            t.set("kind", "vitals_changed")?;
            t.set("health", vitals.health)?;
            t.set("food", vitals.food)?;
            t.set("saturation", vitals.saturation)?;
        }
        HudEvent::ExperienceChanged { experience } => {
            t.set("kind", "experience_changed")?;
            t.set("bar", experience.bar)?;
            t.set("level", experience.level)?;
            t.set("total", experience.total)?;
        }
        HudEvent::GameModeChanged { game_mode } => {
            t.set("kind", "game_mode_changed")?;
            t.set(
                "game_mode",
                crate::lua::convert::hud::game_mode_to_str(game_mode),
            )?;
        }
        HudEvent::AbilitiesChanged { abilities } => {
            t.set("kind", "abilities_changed")?;
            t.set("flying", abilities.flying())?;
            t.set("may_fly", abilities.may_fly())?;
            t.set("invulnerable", abilities.invulnerable())?;
            t.set("instant_build", abilities.instant_build())?;
        }
        HudEvent::HotbarChanged {
            selected_slot,
            held_item,
            applied,
        } => {
            t.set("kind", "hotbar_changed")?;
            t.set("selected_slot", *selected_slot)?;
            t.set(
                "held_item",
                crate::lua::convert::items::raw_slot_to_table(
                    lua,
                    *selected_slot as usize,
                    held_item,
                )?,
            )?;
            t.set("applied", *applied)?;
        }
        HudEvent::CooldownChanged {
            group,
            remaining_ticks,
            applied,
        } => {
            t.set("kind", "cooldown_changed")?;
            t.set("group", group.as_str())?;
            t.set(
                "remaining_ticks",
                match remaining_ticks {
                    Some(v) => Value::Integer(*v as i64),
                    None => Value::Nil,
                },
            )?;
            t.set("applied", *applied)?;
        }
        HudEvent::EffectChanged {
            effect_id,
            current,
            applied,
        } => {
            t.set("kind", "effect_changed")?;
            t.set("effect_id", *effect_id)?;
            t.set(
                "current",
                match current {
                    Some(e) => {
                        let et = lua.create_table()?;
                        et.set("id", e.id)?;
                        et.set("amplifier", e.amplifier)?;
                        et.set("duration_ticks", e.duration_ticks)?;
                        Value::Table(et)
                    }
                    None => Value::Nil,
                },
            )?;
            t.set("applied", *applied)?;
        }
        HudEvent::AttributesChanged { applied, rejected } => {
            t.set("kind", "attributes_changed")?;
            t.set("applied", *applied)?;
            t.set("rejected", *rejected)?;
        }
        HudEvent::Death { information } => {
            t.set("kind", "death")?;
            t.set("player_id", information.player_id)?;
            t.set(
                "message",
                crate::lua::convert::text_component(lua, &information.message)?,
            )?;
        }
        HudEvent::Respawned { state } => {
            t.set("kind", "respawned")?;
            t.set("count", state.count)?;
            t.set("dimension_name", state.dimension_name.as_str())?;
        }
        HudEvent::WorldBorderChanged {
            action,
            current,
            applied,
        } => {
            t.set("kind", "world_border_changed")?;
            t.set(
                "action",
                match action {
                    crate::minecraft::hud::WorldBorderAction::Initialize => "initialize",
                    crate::minecraft::hud::WorldBorderAction::Center => "center",
                    crate::minecraft::hud::WorldBorderAction::LerpSize => "lerp_size",
                    crate::minecraft::hud::WorldBorderAction::Size => "size",
                    crate::minecraft::hud::WorldBorderAction::WarningTime => "warning_time",
                    crate::minecraft::hud::WorldBorderAction::WarningBlocks => "warning_blocks",
                },
            )?;
            t.set("has_border", current.is_some())?;
            t.set("applied", *applied)?;
        }
        HudEvent::TimeChanged { time } => {
            t.set("kind", "time_changed")?;
            t.set("age", time.age)?;
            t.set("day_time", time.day_time)?;
            t.set("ticking", time.ticking)?;
        }
        HudEvent::WeatherChanged { weather } => {
            t.set("kind", "weather_changed")?;
            t.set("raining", weather.raining)?;
            t.set("rain_level", weather.rain_level)?;
            t.set("thunder_level", weather.thunder_level)?;
        }
        HudEvent::DifficultyChanged { difficulty } => {
            t.set("kind", "difficulty_changed")?;
            t.set("locked", difficulty.locked)?;
        }
        HudEvent::SpawnPositionChanged { position } => {
            t.set("kind", "spawn_position_changed")?;
            t.set("x", position.position.x)?;
            t.set("y", position.position.y)?;
            t.set("z", position.position.z)?;
            t.set("angle", position.angle)?;
        }
        HudEvent::PlayerListChanged {
            updated,
            removed,
            rejected,
        } => {
            t.set("kind", "player_list_changed")?;
            t.set("updated_count", updated.len())?;
            t.set("removed_count", removed.len())?;
            t.set("rejected", *rejected)?;
        }
    }
    Ok(t)
}

/// Converts one `BotEvent` into its Lua payload table. The table always has
/// a `.name` field equal to `crate::lua::event::bot_event_name(event)`, so
/// a script that registers one handler for multiple related sub-kinds
/// (e.g. everything under `"hud"`) can still branch on `.kind` inside.
pub fn event_to_table(lua: &Lua, event: &BotEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", crate::lua::event::bot_event_name(event))?;
    match event {
        BotEvent::Login { entity_id } => {
            t.set("entity_id", *entity_id)?;
        }
        BotEvent::Spawned => {}
        BotEvent::Health {
            health,
            food,
            saturation,
        } => {
            t.set("health", *health)?;
            t.set("food", *food)?;
            t.set("saturation", *saturation)?;
        }
        BotEvent::Death => {}
        BotEvent::Chat { sender, message } => {
            t.set("sender", sender.as_str())?;
            t.set("message", message.as_str())?;
        }
        BotEvent::SystemChat { message } => {
            t.set("message", message.as_str())?;
        }
        BotEvent::PlayerJoined { uuid, name } => {
            t.set("uuid", crate::lua::convert::u128_to_hex_string(lua, *uuid)?)?;
            t.set("name", name.as_str())?;
        }
        BotEvent::PlayerLeft { uuid } => {
            t.set("uuid", crate::lua::convert::u128_to_hex_string(lua, *uuid)?)?;
        }
        BotEvent::EntitySpawned {
            entity_id,
            uuid,
            kind,
        } => {
            t.set("entity_id", *entity_id)?;
            t.set("uuid", crate::lua::convert::u128_to_hex_string(lua, *uuid)?)?;
            t.set("kind", *kind)?;
        }
        BotEvent::EntityRemoved { entity_id } => {
            t.set("entity_id", *entity_id)?;
        }
        BotEvent::Time { time_of_day } => {
            t.set("time_of_day", *time_of_day)?;
        }
        BotEvent::Weather { raining } => {
            t.set("raining", *raining)?;
        }
        BotEvent::Kicked { reason } => {
            t.set("reason", reason.as_str())?;
        }
        BotEvent::Presentation(inner) => {
            let payload =
                crate::lua::convert::presentation::presentation_event_to_table(lua, inner)?;
            copy_fields(&t, &payload)?;
        }
        BotEvent::Scoreboard(inner) => {
            let payload = crate::lua::convert::scoreboard::scoreboard_event_to_table(lua, inner)?;
            copy_fields(&t, &payload)?;
        }
        BotEvent::Hud(inner) => {
            let payload = hud_event_to_table(lua, inner)?;
            copy_fields(&t, &payload)?;
        }
        BotEvent::Inventory(inner) => {
            let payload = inventory_event_to_table(lua, inner)?;
            copy_fields(&t, &payload)?;
        }
        BotEvent::Connecting => {}
        BotEvent::Connected => {}
        BotEvent::Disconnected { reason } => {
            t.set("reason", reason.as_str())?;
        }
        BotEvent::ReconnectScheduled { attempt, delay } => {
            t.set("attempt", *attempt)?;
            t.set("delay_ms", crate::lua::convert::duration_ms(*delay))?;
        }
        BotEvent::RetryAttemptStarted { attempt } => {
            t.set("attempt", *attempt)?;
        }
        BotEvent::RetriesExhausted => {}
        BotEvent::StoppedByCancellation => {}
    }
    Ok(t)
}

/// Copies every field of `src` into `dst` without overwriting `dst.name`
/// (already set to the dispatch name by the caller). Used to flatten the
/// boxed sub-event tables (`hud_event_to_table` etc, which set their own
/// `kind`) into the top-level event table so scripts can access both
/// `event.name` (dispatch name) and `event.kind` (sub-variant) directly.
fn copy_fields(dst: &Table, src: &Table) -> mlua::Result<()> {
    for pair in src.pairs::<Value, Value>() {
        let (k, v) = pair?;
        dst.set(k, v)?;
    }
    Ok(())
}

/// `ActionResult` → the table passed to `bot:on("action_result", fn)` and
/// to any one-shot action callback.
pub fn action_result_to_table(lua: &Lua, result: &ActionResult) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("request_id", result.request_id)?;
    t.set("bot_id", result.bot_id)?;
    t.set("outcome", result.outcome.kind())?;
    match &result.outcome {
        ActionOutcome::DeliveredSent => {
            t.set("ok", true)?;
        }
        ActionOutcome::Confirmed { state_id } => {
            t.set("ok", true)?;
            t.set("state_id", *state_id)?;
        }
        ActionOutcome::Corrected { state_id } => {
            t.set("ok", true)?;
            t.set("state_id", *state_id)?;
        }
        ActionOutcome::Error(err) => {
            t.set("ok", false)?;
            t.set(
                "error",
                crate::lua::convert::events::script_error_to_table(lua, err)?,
            )?;
        }
    }
    Ok(t)
}

pub fn script_error_to_table(
    lua: &Lua,
    err: &crate::lua::error::ScriptError,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("code", err.code)?;
    t.set("message", err.message.as_str())?;
    t.set("retryable", err.retryable)?;
    t.set(
        "bot_id",
        match err.bot_id {
            Some(id) => Value::Integer(id as i64),
            None => Value::Nil,
        },
    )?;
    t.set(
        "generation",
        match err.generation {
            Some(g) => Value::Integer(g as i64),
            None => Value::Nil,
        },
    )?;
    Ok(t)
}
