//! Parses the Lua tables passed to `swarm:add_server/add_proxy/add_bot/
//! add_group` (and their `reconnect = {...}` sub-tables) into
//! `crate::lua::registry`'s pure-Rust config types. All parsing happens
//! here so `registry.rs` itself never depends on `mlua`.

use std::time::Duration;

use mlua::{Table, Value};

use crate::core::supervisor::{Jitter, ReconnectPolicy, RetryDecision, RetryLimit};
use crate::lua::error::ScriptError;
use crate::lua::registry::{BotSpec, ProxyDef, ServerDef};

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

fn get_u16(table: &Table, key: &str, default: u16) -> mlua::Result<u16> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(i) => Ok(i as u16),
        Value::Number(n) => Ok(n as u16),
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a number, got {}",
            other.type_name()
        ))),
    }
}

fn get_i8(table: &Table, key: &str, default: i8) -> mlua::Result<i8> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(i) => Ok(i as i8),
        Value::Number(n) => Ok(n as i8),
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a number, got {}",
            other.type_name()
        ))),
    }
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

fn get_ms(table: &Table, key: &str, default: Duration) -> mlua::Result<Duration> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(i) => Ok(Duration::from_millis(i.max(0) as u64)),
        Value::Number(n) => Ok(Duration::from_millis(n.max(0.0) as u64)),
        other => Err(mlua::Error::RuntimeError(format!(
            "field `{key}` must be a number of milliseconds, got {}",
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
    def.view_distance = get_i8(table, "view_distance", def.view_distance)?;
    def.write_timeout = get_ms(table, "write_timeout_ms", def.write_timeout)?;
    def.connect_deadline = get_ms(table, "connect_deadline_ms", def.connect_deadline)?;
    def.shared_chunks = get_bool(table, "shared_chunks", def.shared_chunks)?;
    Ok(def)
}

pub fn parse_proxy_def(table: &Table) -> mlua::Result<ProxyDef> {
    Ok(ProxyDef {
        name: get_string(table, "name")?.unwrap_or_default(),
        host: get_string(table, "host")?.unwrap_or_default(),
        port: get_u16(table, "port", 1080)?,
        username_env: get_string(table, "username_env")?,
        password_env: get_string(table, "password_env")?,
    })
}

fn parse_retry_decision(table: &Table, key: &str, default: RetryDecision) -> mlua::Result<RetryDecision> {
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
        Value::Integer(i) => RetryLimit::Count(i.max(0) as u32),
        Value::Number(n) => RetryLimit::Count(n.max(0.0) as u32),
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "field `max_retries` must be a count or \"unlimited\", got {}",
                other.type_name()
            )))
        }
    };
    policy.initial_delay = get_ms(table, "initial_delay_ms", policy.initial_delay)?;
    policy.max_delay = get_ms(table, "max_delay_ms", policy.max_delay)?;
    if let Value::Number(n) = table.get::<Value>("multiplier")? {
        policy.multiplier = n;
    } else if let Value::Integer(i) = table.get::<Value>("multiplier")? {
        policy.multiplier = i as f64;
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
    policy.stable_session_reset = get_ms(table, "stable_session_reset_ms", policy.stable_session_reset)?;
    policy.on_transient = parse_retry_decision(table, "on_transient", policy.on_transient)?;
    policy.on_server_rejected = parse_retry_decision(table, "on_server_rejected", policy.on_server_rejected)?;
    policy.on_auth_failure = parse_retry_decision(table, "on_auth_failure", policy.on_auth_failure)?;
    policy.on_protocol_incompatible =
        parse_retry_decision(table, "on_protocol_incompatible", policy.on_protocol_incompatible)?;
    Ok(policy)
}

pub fn parse_bot_spec(table: &Table) -> mlua::Result<BotSpec> {
    let id = match table.get::<Value>("id")? {
        Value::Nil => None,
        Value::Integer(i) => Some(i.max(0) as u32),
        Value::Number(n) => Some(n.max(0.0) as u32),
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
