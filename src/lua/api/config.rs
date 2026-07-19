//! Parses the Lua tables passed to `swarm:add_server/add_bot/add_group`
//! (and their `reconnect = {...}` sub-tables) into `crate::lua::registry`'s
//! pure-Rust config types. All parsing happens here so `registry.rs`
//! itself never depends on `mlua`.
//!
//! Deliberately no `parse_proxy_def`: proxy profiles are host-supplied,
//! never Lua-constructible — see `crate::lua::registry`'s module doc
//! comment.

use std::time::Duration;

use mlua::{Table, Value};

use crate::core::supervisor::{Jitter, ReconnectPolicy, RetryDecision, RetryLimit};
use crate::lua::error::ScriptError;
use crate::lua::registry::{BotSpec, ServerDef};

fn get_string(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a string, got {}",
            other.type_name()
        ))),
    }
}

/// Delegates to `mlua`'s own `FromLua for u16`, which already performs a
/// checked (never-wrapping) conversion — out-of-range or non-integer
/// input is a clear `mlua::Error`, never a silently-truncated `as u16`
/// cast (e.g. a script-supplied `70000` would previously wrap to `4464`
/// instead of being rejected).
fn get_u16(table: &Table, key: &str, default: u16) -> mlua::Result<u16> {
    Ok(table.get::<Option<u16>>(key)?.unwrap_or(default))
}

/// Same as [`get_u16`], but checked-narrowing to `i8` — the caller
/// (`parse_server_def`) then range-checks the *correctly parsed* value
/// against `2..=32`, rather than that check running against a value an
/// unchecked `as i8` cast may have already silently wrapped into some
/// unrelated in-range number (e.g. `266 as i8 == 10`, which `2..=32`
/// would then wrongly accept as if the script had asked for `10`).
fn get_i8(table: &Table, key: &str, default: i8) -> mlua::Result<i8> {
    Ok(table.get::<Option<i8>>(key)?.unwrap_or(default))
}

fn get_bool(table: &Table, key: &str, default: bool) -> mlua::Result<bool> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Boolean(b) => Ok(b),
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a boolean, got {}",
            other.type_name()
        ))),
    }
}

/// Parses a non-negative millisecond duration. Unlike `get_u16`/`get_i8`,
/// this can't just delegate to a built-in `FromLua` impl (there is no
/// `Duration` one) — rejects a negative or non-finite (`NaN`/`Infinity`)
/// Lua number outright instead of the previous `n.max(0.0)`-style silent
/// clamp-to-zero (which could mask a script bug behind an unexpectedly
/// instant timeout) or letting an infinity silently saturate into an
/// absurd multi-million-year `Duration`.
fn get_ms(table: &Table, key: &str, default: Duration) -> mlua::Result<Duration> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(i) => {
            if i < 0 {
                return Err(mlua::Error::RuntimeError(format!(
                    "field `{key}` must not be negative, got {i}"
                )));
            }
            Ok(Duration::from_millis(i as u64))
        }
        Value::Number(n) => {
            if !n.is_finite() || n < 0.0 {
                return Err(mlua::Error::RuntimeError(format!(
                    "field `{key}` must be a finite, non-negative number of milliseconds, got {n}"
                )));
            }
            Ok(Duration::from_millis(n as u64))
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a number of milliseconds, got {}",
            other.type_name()
        ))),
    }
}

/// Checked, never-silently-clamping parse of a non-negative 32-bit count
/// (a bot id, a retry count, …) from an already-fetched `Value` — shared
/// by `parse_bot_spec`'s `id` field and `parse_reconnect_policy`'s
/// `max_retries` numeric branch. A negative integer or a non-finite/
/// out-of-range/fractional-looking-but-still-huge float is a typed
/// error, never silently coerced (the previous `i.max(0) as u32` /
/// `n.max(0.0) as u32` pattern this replaces).
fn checked_u32_from_value(value: &Value, key: &str) -> mlua::Result<u32> {
    match value {
        Value::Integer(i) => u32::try_from(*i).map_err(|_| {
            mlua::Error::RuntimeError(format!(
                "field `{key}` must be a non-negative integer that fits in 32 bits, got {i}"
            ))
        }),
        Value::Number(n) => {
            if !n.is_finite() || *n < 0.0 || *n > u32::MAX as f64 {
                Err(mlua::Error::RuntimeError(format!(
                    "field `{key}` must be a non-negative integer that fits in 32 bits, got {n}"
                )))
            } else {
                Ok(*n as u32)
            }
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a number, got {}",
            other.type_name()
        ))),
    }
}

pub fn parse_server_def(table: &Table) -> mlua::Result<ServerDef> {
    let mut def = ServerDef {
        name: get_string(table, "name")?.unwrap_or_default(),
        host: get_string(table, "host")?.unwrap_or_default(),
        ..ServerDef::default()
    };
    def.port = get_u16(table, "port", def.port)?;
    if def.port == 0 {
        return Err(mlua::Error::RuntimeError(
            "field `port` must be in 1..=65535, got 0".to_string(),
        ));
    }
    def.view_distance = get_i8(table, "view_distance", def.view_distance)?;
    def.write_timeout = get_ms(table, "write_timeout_ms", def.write_timeout)?;
    def.connect_deadline = get_ms(table, "connect_deadline_ms", def.connect_deadline)?;
    def.shared_chunks = get_bool(table, "shared_chunks", def.shared_chunks)?;
    Ok(def)
}

fn parse_retry_decision(
    table: &Table,
    key: &str,
    default: RetryDecision,
) -> mlua::Result<RetryDecision> {
    match get_string(table, key)? {
        None => Ok(default),
        Some(s) if s == "retry" => Ok(RetryDecision::Retry),
        Some(s) if s == "stop" => Ok(RetryDecision::Stop),
        Some(other) => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be \"retry\" or \"stop\", got \"{other}\""
        ))),
    }
}

pub fn parse_reconnect_policy(table: Option<&Table>) -> mlua::Result<ReconnectPolicy> {
    let mut policy = ReconnectPolicy::default();
    let Some(table) = table else {
        return Ok(policy);
    };
    policy.enabled = get_bool(table, "enabled", policy.enabled)?;
    policy.max_retries = match table.get::<Value>("max_retries")? {
        Value::Nil => policy.max_retries,
        Value::String(s) if s.to_str()?.as_ref() == "unlimited" => RetryLimit::Unlimited,
        v @ (Value::Integer(_) | Value::Number(_)) => {
            RetryLimit::Count(checked_u32_from_value(&v, "max_retries")?)
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "field `max_retries` must be a count or \"unlimited\", got {}",
                other.type_name()
            )))
        }
    };
    policy.initial_delay = get_ms(table, "initial_delay_ms", policy.initial_delay)?;
    policy.max_delay = get_ms(table, "max_delay_ms", policy.max_delay)?;
    // A non-finite or non-positive multiplier would corrupt the
    // exponential-backoff calculation in `crate::core::supervisor`
    // (e.g. a `0`/negative multiplier never grows the delay at all; NaN
    // propagates through every subsequent computation) — reject it
    // outright rather than silently accepting whatever was given.
    match table.get::<Value>("multiplier")? {
        Value::Nil => {}
        Value::Integer(i) if i > 0 => policy.multiplier = i as f64,
        Value::Number(n) if n.is_finite() && n > 0.0 => policy.multiplier = n,
        Value::Integer(_) | Value::Number(_) => {
            return Err(mlua::Error::RuntimeError(
                "field `multiplier` must be a finite, positive number".to_string(),
            ))
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "field `multiplier` must be a number, got {}",
                other.type_name()
            )))
        }
    }
    if let Value::Table(jitter) = table.get::<Value>("jitter")? {
        let kind = get_string(&jitter, "type")?.unwrap_or_else(|| "none".to_string());
        policy.jitter = if kind == "deterministic" {
            let fraction = match jitter.get::<Value>("fraction")? {
                Value::Number(n) => n,
                Value::Integer(i) => i as f64,
                _ => 0.1,
            };
            Jitter::Deterministic(fraction)
        } else {
            Jitter::None
        };
    }
    policy.stable_session_reset = get_ms(
        table,
        "stable_session_reset_ms",
        policy.stable_session_reset,
    )?;
    policy.on_transient = parse_retry_decision(table, "on_transient", policy.on_transient)?;
    policy.on_server_rejected =
        parse_retry_decision(table, "on_server_rejected", policy.on_server_rejected)?;
    policy.on_auth_failure =
        parse_retry_decision(table, "on_auth_failure", policy.on_auth_failure)?;
    policy.on_protocol_incompatible = parse_retry_decision(
        table,
        "on_protocol_incompatible",
        policy.on_protocol_incompatible,
    )?;
    Ok(policy)
}

pub fn parse_bot_spec(table: &Table) -> mlua::Result<BotSpec> {
    let id = match table.get::<Value>("id")? {
        Value::Nil => None,
        v @ (Value::Integer(_) | Value::Number(_)) => Some(checked_u32_from_value(&v, "id")?),
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "field `id` must be a number, got {}",
                other.type_name()
            )))
        }
    };
    let reconnect_table = match table.get::<Value>("reconnect")? {
        Value::Table(t) => Some(t),
        _ => None,
    };
    Ok(BotSpec {
        id,
        username: get_string(table, "username")?.unwrap_or_default(),
        server: get_string(table, "server")?.unwrap_or_default(),
        proxy: get_string(table, "proxy")?,
        reconnect: parse_reconnect_policy(reconnect_table.as_ref())?,
        label: get_string(table, "id_prefix")?,
    })
}

pub fn to_lua_error(err: ScriptError) -> mlua::Error {
    mlua::Error::RuntimeError(format!("{}: {}", err.code, err.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_table(lua: &mlua::Lua, fields: &[(&str, Value)]) -> Table {
        let t = lua.create_table().unwrap();
        t.set("name", "main").unwrap();
        t.set("host", "127.0.0.1").unwrap();
        for (k, v) in fields {
            t.set(*k, v.clone()).unwrap();
        }
        t
    }

    #[test]
    fn port_zero_is_rejected() {
        let lua = mlua::Lua::new();
        let t = server_table(&lua, &[("port", Value::Integer(0))]);
        let err = parse_server_def(&t).unwrap_err();
        assert!(err.to_string().contains("1..=65535"));
    }

    #[test]
    fn port_above_65535_is_rejected_not_silently_wrapped() {
        let lua = mlua::Lua::new();
        // 70000 as u16 would previously wrap to 4464 via an unchecked
        // `as u16` cast, silently accepting a completely different port.
        let t = server_table(&lua, &[("port", Value::Integer(70_000))]);
        let err = parse_server_def(&t).unwrap_err();
        assert!(err.to_string().contains("out of range") || err.to_string().contains("65535"));
    }

    #[test]
    fn port_negative_is_rejected() {
        let lua = mlua::Lua::new();
        let t = server_table(&lua, &[("port", Value::Integer(-1))]);
        assert!(parse_server_def(&t).is_err());
    }

    #[test]
    fn view_distance_far_out_of_range_is_rejected_not_silently_wrapped_into_range() {
        let lua = mlua::Lua::new();
        // 266 as i8 wraps to 10 via an unchecked `as i8` cast — 10 IS
        // within the later `2..=32` check, so the *old* code would have
        // silently accepted "266" as if the script had asked for "10".
        // The checked conversion must reject 266 outright, before that
        // check ever runs.
        let t = server_table(
            &lua,
            &[
                ("port", Value::Integer(25565)),
                ("view_distance", Value::Integer(266)),
            ],
        );
        assert!(parse_server_def(&t).is_err());
    }

    #[test]
    fn write_timeout_negative_is_rejected_not_clamped_to_zero() {
        let lua = mlua::Lua::new();
        let t = server_table(
            &lua,
            &[
                ("port", Value::Integer(25565)),
                ("write_timeout_ms", Value::Integer(-500)),
            ],
        );
        let err = parse_server_def(&t).unwrap_err();
        assert!(err.to_string().contains("negative"));
    }

    #[test]
    fn connect_deadline_nan_is_rejected_not_silently_coerced_to_zero() {
        let lua = mlua::Lua::new();
        let t = server_table(
            &lua,
            &[
                ("port", Value::Integer(25565)),
                ("connect_deadline_ms", Value::Number(f64::NAN)),
            ],
        );
        let err = parse_server_def(&t).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn connect_deadline_infinity_is_rejected_not_saturated_into_an_absurd_duration() {
        let lua = mlua::Lua::new();
        let t = server_table(
            &lua,
            &[
                ("port", Value::Integer(25565)),
                ("connect_deadline_ms", Value::Number(f64::INFINITY)),
            ],
        );
        let err = parse_server_def(&t).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn a_valid_server_def_still_parses_normally() {
        let lua = mlua::Lua::new();
        let t = server_table(&lua, &[("port", Value::Integer(25565))]);
        let def = parse_server_def(&t).unwrap();
        assert_eq!(def.port, 25565);
        assert_eq!(def.name, "main");
    }

    #[test]
    fn bot_id_negative_is_rejected_not_clamped_to_zero() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("username", "alice").unwrap();
        t.set("server", "main").unwrap();
        t.set("id", -5).unwrap();
        let err = parse_bot_spec(&t).unwrap_err();
        assert!(err.to_string().contains("non-negative"));
    }

    #[test]
    fn bot_id_fits_and_parses_normally() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("username", "alice").unwrap();
        t.set("server", "main").unwrap();
        t.set("id", 42).unwrap();
        let spec = parse_bot_spec(&t).unwrap();
        assert_eq!(spec.id, Some(42));
    }

    #[test]
    fn reconnect_multiplier_non_positive_is_rejected() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("multiplier", 0).unwrap();
        let err = parse_reconnect_policy(Some(&t)).unwrap_err();
        assert!(err.to_string().contains("positive"));
    }

    #[test]
    fn reconnect_multiplier_nan_is_rejected() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("multiplier", f64::NAN).unwrap();
        let err = parse_reconnect_policy(Some(&t)).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    #[test]
    fn reconnect_max_retries_negative_is_rejected_not_clamped_to_zero() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("max_retries", -3).unwrap();
        assert!(parse_reconnect_policy(Some(&t)).is_err());
    }

    #[test]
    fn reconnect_max_retries_unlimited_string_still_works() {
        let lua = mlua::Lua::new();
        let t = lua.create_table().unwrap();
        t.set("max_retries", "unlimited").unwrap();
        let policy = parse_reconnect_policy(Some(&t)).unwrap();
        assert_eq!(policy.max_retries, RetryLimit::Unlimited);
    }
}
