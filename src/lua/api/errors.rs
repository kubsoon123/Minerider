//! Helpers for returning `crate::lua::error::ScriptError` to Lua as
//! `(nil, error_table)` — the mission's convention
//! (`local request_id, err = bot:forward(true)`), so every wrapped action
//! returns either a real value or a stable `{code, message, retryable,
//! bot_id, generation}` table, never a raised Lua error for expected
//! failure modes.

use mlua::{Lua, Value};

use crate::lua::error::ScriptError;

/// `(Value::Nil, error_table)` — the standard failure return shape.
pub fn err_pair(lua: &Lua, err: ScriptError) -> mlua::Result<(Value, Value)> {
    let table = crate::lua::convert::events::script_error_to_table(lua, &err)?;
    Ok((Value::Nil, Value::Table(table)))
}

/// `(value, Value::Nil)` — the standard success return shape.
pub fn ok_pair(value: Value) -> (Value, Value) {
    (value, Value::Nil)
}
