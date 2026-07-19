//! Rust → Lua table conversions for every state/event type the wrapper
//! exposes. Every conversion produces a **detached** Lua table: mutating it
//! from Lua can never affect Rust state, and no full chunk/world payload is
//! ever copied in (see `docs/lua_api_reference.md#state`).

pub mod events;
pub mod gui;
pub mod hud;
pub mod items;
pub mod presentation;
pub mod scoreboard;
pub mod state;

use mlua::{Lua, Table, Value};

pub fn u128_to_hex_string(lua: &Lua, value: u128) -> mlua::Result<Value> {
    Ok(Value::String(lua.create_string(format!("{value:032x}"))?))
}

pub fn opt_string(lua: &Lua, value: &Option<String>) -> mlua::Result<Value> {
    match value {
        Some(s) => Ok(Value::String(lua.create_string(s)?)),
        None => Ok(Value::Nil),
    }
}

pub fn duration_ms(d: std::time::Duration) -> i64 {
    d.as_millis() as i64
}

pub fn string_array(
    lua: &Lua,
    items: impl IntoIterator<Item = impl AsRef<str>>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (i, s) in items.into_iter().enumerate() {
        t.set(i + 1, s.as_ref())?;
    }
    Ok(t)
}

/// Text components are exposed as their best-effort plain-text rendering
/// only (`.plain_text()`) — not the full color/style/click-event tree.
/// Documented as a simplification in `docs/lua_api_reference.md#limitations`.
pub fn text_component(
    lua: &Lua,
    text: &crate::minecraft::text::TextComponent,
) -> mlua::Result<Value> {
    Ok(Value::String(lua.create_string(text.plain_text())?))
}

pub fn opt_text_component(
    lua: &Lua,
    text: &Option<crate::minecraft::text::TextComponent>,
) -> mlua::Result<Value> {
    match text {
        Some(t) => text_component(lua, t),
        None => Ok(Value::Nil),
    }
}
