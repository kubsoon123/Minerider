//! `StateSnapshot` → Lua table conversion: the read-only detached snapshot
//! behind `bot:state()` and the targeted views (`bot:player()`,
//! `bot:entities()`, `bot:players()`, `bot:inventory()`).

use mlua::{Lua, Table, Value};

use crate::minecraft::entity::{Entity, EntityStore};
use crate::minecraft::inventory::{InventoryState, Window};
use crate::minecraft::play::StateSnapshot;
use crate::minecraft::player::{LocalPlayer, PlayerPosition};
use crate::minecraft::players::{PlayerEntry, PlayerList};

pub fn opt_i32(v: Option<i32>) -> Value {
    match v {
        Some(v) => Value::Integer(v as i64),
        None => Value::Nil,
    }
}

pub fn state_snapshot_to_table(lua: &Lua, snapshot: &StateSnapshot) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("tick", snapshot.tick)?;
    t.set("player", player_to_table(lua, &snapshot.player)?)?;
    t.set("entities", entities_to_table(lua, &snapshot.entities)?)?;
    t.set("inventory", inventory_state_to_table(lua, &snapshot.inventory)?)?;
    t.set("players", players_to_table(lua, &snapshot.players)?)?;
    t.set(
        "presentation",
        super::presentation::presentation_state_to_table(lua, &snapshot.presentation)?,
    )?;
    t.set(
        "scoreboard",
        super::scoreboard::scoreboard_state_to_table(lua, &snapshot.scoreboard)?,
    )?;
    t.set("hud", super::hud::hud_state_to_table(lua, &snapshot.hud)?)?;
    t.set("world_time", snapshot.world_time)?;
    t.set("raining", snapshot.raining)?;
    t.set(
        "dimension",
        match &snapshot.dimension {
            Some(dim) => Value::String(lua.create_string(dim.key.as_str())?),
            None => Value::Nil,
        },
    )?;
    Ok(t)
}

pub fn player_position_to_table(lua: &Lua, pos: &PlayerPosition) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("x", pos.x)?;
    t.set("y", pos.y)?;
    t.set("z", pos.z)?;
    t.set("yaw", pos.yaw)?;
    t.set("pitch", pos.pitch)?;
    Ok(t)
}

pub fn player_to_table(lua: &Lua, player: &LocalPlayer) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("entity_id", opt_i32(player.entity_id))?;
    t.set("position", player_position_to_table(lua, &player.position)?)?;
    t.set("health", player.health)?;
    t.set("food", player.food)?;
    t.set("saturation", player.saturation)?;
    t.set("xp_bar", player.xp_bar)?;
    t.set("xp_level", player.xp_level)?;
    t.set("total_experience", player.total_experience)?;
    t.set("loaded", player.loaded)?;
    t.set("on_ground", player.on_ground)?;
    t.set("horizontal_collision", player.horizontal_collision)?;
    let velocity = lua.create_table()?;
    velocity.set("x", player.velocity.x)?;
    velocity.set("y", player.velocity.y)?;
    velocity.set("z", player.velocity.z)?;
    t.set("velocity", velocity)?;
    let input = lua.create_table()?;
    input.set("forward", player.input.forward)?;
    input.set("strafe", player.input.strafe)?;
    input.set("jump", player.input.jump)?;
    input.set("sprint", player.input.sprint)?;
    input.set("sneak", player.input.sneak)?;
    t.set("input", input)?;
    Ok(t)
}

pub fn entity_to_table(lua: &Lua, entity: &Entity) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", entity.id)?;
    t.set("uuid", super::u128_to_hex_string(lua, entity.uuid)?)?;
    t.set("kind", entity.kind)?;
    t.set("x", entity.x)?;
    t.set("y", entity.y)?;
    t.set("z", entity.z)?;
    t.set("yaw", entity.yaw)?;
    t.set("pitch", entity.pitch)?;
    t.set("head_yaw", entity.head_yaw)?;
    t.set("vx", entity.vx)?;
    t.set("vy", entity.vy)?;
    t.set("vz", entity.vz)?;
    t.set("on_ground", entity.on_ground)?;
    Ok(t)
}

/// Bounded snapshot/poll — never a per-tick or per-movement event stream
/// (see `docs/lua_api_reference.md#entities`).
pub fn entities_to_table(lua: &Lua, entities: &EntityStore) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for entity in entities.iter() {
        t.set(entity.id, entity_to_table(lua, entity)?)?;
    }
    Ok(t)
}

pub fn player_entry_to_table(lua: &Lua, entry: &PlayerEntry) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("uuid", super::u128_to_hex_string(lua, entry.uuid)?)?;
    t.set("name", entry.name.as_str())?;
    t.set("gamemode", entry.gamemode)?;
    t.set("latency", entry.latency)?;
    t.set("listed", entry.listed)?;
    t.set("display_name", super::opt_text_component(lua, &entry.display_name)?)?;
    t.set("list_priority", entry.list_priority)?;
    t.set("show_hat", entry.show_hat)?;
    t.set(
        "chat_session",
        match &entry.chat_session {
            Some(session) => {
                let st = lua.create_table()?;
                st.set("session_id", super::u128_to_hex_string(lua, session.session_id)?)?;
                st.set("expires_at_millis", session.expires_at_millis)?;
                Value::Table(st)
            }
            None => Value::Nil,
        },
    )?;
    Ok(t)
}

/// No secret auth data is ever included — only the same bounded tab-list
/// fields `PlayerEntry` tracks.
pub fn players_to_table(lua: &Lua, players: &PlayerList) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for entry in players.iter() {
        t.set(
            super::u128_to_hex_string(lua, entry.uuid)?,
            player_entry_to_table(lua, entry)?,
        )?;
    }
    Ok(t)
}

pub fn window_to_table(lua: &Lua, window: &Window) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", window.id)?;
    t.set("kind", window.kind)?;
    t.set("state_id", window.state_id)?;
    let slots = lua.create_table()?;
    for (i, slot) in window.slots.iter().enumerate() {
        slots.set(i + 1, super::items::raw_slot_to_table(lua, i, slot)?)?;
    }
    t.set("slots", slots)?;
    t.set("slots_truncated", window.slots_truncated)?;
    let properties = lua.create_table()?;
    for (k, v) in &window.properties {
        properties.set(*k, *v)?;
    }
    t.set("properties", properties)?;
    Ok(t)
}

pub fn inventory_state_to_table(lua: &Lua, inv: &InventoryState) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("player_inventory", window_to_table(lua, &inv.player_inventory)?)?;
    t.set(
        "open_window",
        match &inv.open_window {
            Some(w) => Value::Table(window_to_table(lua, w)?),
            None => Value::Nil,
        },
    )?;
    t.set("cursor", super::items::raw_slot_to_table(lua, 0, &inv.cursor)?)?;
    t.set("selected_hotbar_slot", inv.selected_hotbar_slot)?;
    let pending: Vec<u64> = inv.pending_transactions.iter().map(|tx| tx.request.transaction_id).collect();
    t.set("pending_transaction_ids", super_u64_array(lua, &pending)?)?;
    Ok(t)
}

fn super_u64_array(lua: &Lua, items: &[u64]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (i, v) in items.iter().enumerate() {
        t.set(i + 1, *v)?;
    }
    Ok(t)
}
