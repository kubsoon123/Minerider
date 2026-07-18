//! `swarm.shared:set/get/update(key, fn)` — Rust-owned, bounded,
//! cross-worker key/value state, plus bounded pub/sub
//! (`swarm:publish`/`swarm:on_message`).
//!
//! Only [`SharedValue`]-representable data crosses the boundary — every
//! Lua value is deep-copied through `SharedValue` on the way in and out,
//! so no two workers ever share a live reference into another VM. The
//! shared-state mutex is **never** held during arbitrary Lua execution:
//! `update` reads a clone under the lock, releases it, runs the Lua
//! updater function, then re-acquires the lock to compare-and-swap
//! (retrying on a concurrent writer) — see [`SharedState::update`].

use std::collections::HashMap;
use std::sync::Mutex;

use mlua::{Lua, Table, Value};

use crate::lua::shared_value::{
    SharedValue, SharedValueError, MAX_ARRAY_LEN, MAX_DEPTH, MAX_MAP_KEYS, MAX_STRING_LEN,
    MAX_TOTAL_SIZE,
};

/// Total number of distinct keys the shared store will hold.
pub const MAX_STORE_KEYS: usize = 4096;

struct Versioned {
    value: SharedValue,
    version: u64,
}

#[derive(Default)]
pub struct SharedState {
    store: Mutex<HashMap<String, Versioned>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SharedStateError {
    #[error("shared value error: {0}")]
    Value(#[from] SharedValueError),
    #[error("shared store is full (max {MAX_STORE_KEYS} keys)")]
    StoreFull,
    #[error("update was retried too many times under concurrent writers")]
    UpdateContention,
}

impl SharedState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<SharedValue> {
        self.store.lock().expect("shared state poisoned").get(key).map(|v| v.value.clone())
    }

    pub fn set(&self, key: String, value: SharedValue) -> Result<(), SharedStateError> {
        let mut guard = self.store.lock().expect("shared state poisoned");
        if !guard.contains_key(&key) && guard.len() >= MAX_STORE_KEYS {
            return Err(SharedStateError::StoreFull);
        }
        let version = guard.get(&key).map(|v| v.version + 1).unwrap_or(0);
        guard.insert(key, Versioned { value, version });
        Ok(())
    }

    /// Atomic single-key update: reads a clone under the lock, releases it,
    /// then the caller runs an arbitrary (Lua) function against that clone
    /// with the lock **not** held, and calls [`Self::compare_and_swap`]
    /// with the version this read observed.
    pub fn read_for_update(&self, key: &str) -> (SharedValue, u64) {
        let guard = self.store.lock().expect("shared state poisoned");
        match guard.get(key) {
            Some(v) => (v.value.clone(), v.version),
            None => (SharedValue::Nil, 0),
        }
    }

    /// Retries are the caller's responsibility (see `crate::lua::api::bot`'s
    /// `shared:update` wiring) — this just reports whether the compare
    /// succeeded so a bounded retry loop can re-run the updater on failure.
    pub fn compare_and_swap(
        &self,
        key: &str,
        expected_version: u64,
        new_value: SharedValue,
    ) -> Result<bool, SharedStateError> {
        let mut guard = self.store.lock().expect("shared state poisoned");
        let current_version = guard.get(key).map(|v| v.version).unwrap_or(0);
        if current_version != expected_version {
            return Ok(false);
        }
        if !guard.contains_key(key) && guard.len() >= MAX_STORE_KEYS {
            return Err(SharedStateError::StoreFull);
        }
        guard.insert(
            key.to_string(),
            Versioned {
                value: new_value,
                version: expected_version + 1,
            },
        );
        Ok(true)
    }
}

/// Converts an `mlua::Value` into a [`SharedValue`], enforcing every bound
/// (depth/string length/array length/key count/total size) and rejecting
/// functions, userdata, threads, and non-finite numbers. A self-referential
/// Lua table is rejected via the depth bound, not separate cycle detection
/// — bounded recursion cannot loop forever.
pub fn lua_value_to_shared(value: &Value) -> Result<SharedValue, SharedValueError> {
    let mut budget = MAX_TOTAL_SIZE;
    convert(value, 0, &mut budget)
}

fn convert(value: &Value, depth: usize, budget: &mut usize) -> Result<SharedValue, SharedValueError> {
    if depth > MAX_DEPTH {
        return Err(SharedValueError::TooDeep);
    }
    let result = match value {
        Value::Nil => SharedValue::Nil,
        Value::Boolean(b) => SharedValue::Bool(*b),
        Value::Integer(i) => SharedValue::Number(*i as f64),
        Value::Number(n) => {
            if !n.is_finite() {
                return Err(SharedValueError::NonFiniteNumber);
            }
            SharedValue::Number(*n)
        }
        Value::String(s) => {
            let bytes = s.as_bytes();
            if bytes.len() > MAX_STRING_LEN {
                return Err(SharedValueError::StringTooLong);
            }
            SharedValue::Str(String::from_utf8_lossy(&bytes).to_string())
        }
        Value::Table(t) => convert_table(t, depth, budget)?,
        Value::Function(_) | Value::UserData(_) | Value::Thread(_) | Value::LightUserData(_) | Value::Error(_) => {
            return Err(SharedValueError::UnsupportedType("function/userdata/thread"));
        }
        _ => return Err(SharedValueError::UnsupportedType("unrecognized")),
    };
    let size = result.approx_size();
    if size > *budget {
        return Err(SharedValueError::TooLarge);
    }
    *budget = budget.saturating_sub(size.min(*budget));
    Ok(result)
}

fn convert_table(t: &Table, depth: usize, budget: &mut usize) -> Result<SharedValue, SharedValueError> {
    // A dense 1-based integer sequence converts to an array; anything else
    // (string keys, sparse/mixed keys) converts to a map. Non-string,
    // non-sequence-index keys are rejected outright.
    let len = t.raw_len();
    let is_array = len > 0 && {
        let mut count = 0usize;
        for pair in t.clone().pairs::<Value, Value>() {
            let (k, _) = pair.map_err(|_| SharedValueError::UnsupportedKey)?;
            count += 1;
            if !matches!(&k, Value::Integer(i) if *i >= 1 && *i as usize <= len) {
                return convert_map(t, depth, budget);
            }
        }
        count == len
    };
    if is_array {
        if len > MAX_ARRAY_LEN {
            return Err(SharedValueError::ArrayTooLong);
        }
        let mut items = Vec::with_capacity(len);
        for i in 1..=len {
            let v: Value = t.get(i).map_err(|_| SharedValueError::UnsupportedKey)?;
            items.push(convert(&v, depth + 1, budget)?);
        }
        Ok(SharedValue::Array(items))
    } else {
        convert_map(t, depth, budget)
    }
}

fn convert_map(t: &Table, depth: usize, budget: &mut usize) -> Result<SharedValue, SharedValueError> {
    let mut entries = Vec::new();
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair.map_err(|_| SharedValueError::UnsupportedKey)?;
        let key = match k {
            Value::String(s) => String::from_utf8_lossy(&s.as_bytes()).to_string(),
            Value::Integer(i) => i.to_string(),
            _ => return Err(SharedValueError::UnsupportedKey),
        };
        entries.push((key, convert(&v, depth + 1, budget)?));
        if entries.len() > MAX_MAP_KEYS {
            return Err(SharedValueError::TooManyKeys);
        }
    }
    Ok(SharedValue::Map(entries))
}

pub fn shared_value_to_lua(lua: &Lua, value: &SharedValue) -> mlua::Result<Value> {
    Ok(match value {
        SharedValue::Nil => Value::Nil,
        SharedValue::Bool(b) => Value::Boolean(*b),
        SharedValue::Number(n) => Value::Number(*n),
        SharedValue::Str(s) => Value::String(lua.create_string(s)?),
        SharedValue::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(i + 1, shared_value_to_lua(lua, item)?)?;
            }
            Value::Table(t)
        }
        SharedValue::Map(entries) => {
            let t = lua.create_table()?;
            for (k, v) in entries {
                t.set(k.as_str(), shared_value_to_lua(lua, v)?)?;
            }
            Value::Table(t)
        }
    })
}

/// `swarm.shared` — obtained via `LuaSwarm`'s `shared` field getter, one
/// per worker but all backed by the same process-wide `Arc<SharedState>`.
#[derive(Clone)]
pub struct LuaSharedHandle {
    pub shared: std::sync::Arc<SharedState>,
}

const MAX_UPDATE_RETRIES: u32 = 8;

impl mlua::UserData for LuaSharedHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, key: String| {
            match this.shared.get(&key) {
                Some(v) => shared_value_to_lua(lua, &v),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("set", |lua, this, (key, value): (String, Value)| {
            match lua_value_to_shared(&value) {
                Ok(shared) => match this.shared.set(key, shared) {
                    Ok(()) => Ok((Value::Boolean(true), Value::Nil)),
                    Err(e) => crate::lua::api::errors::err_pair(
                        lua,
                        crate::lua::error::ScriptError::new("invalid_configuration", e.to_string()),
                    ),
                },
                Err(e) => crate::lua::api::errors::err_pair(
                    lua,
                    crate::lua::error::ScriptError::new("invalid_configuration", e.to_string()),
                ),
            }
        });
        // `update(key, fn)`: fn receives the current value (or nil) and
        // must return the new value. The store mutex is held only for the
        // read and the final compare-and-swap — never while `fn` itself
        // runs, so one worker's updater can never block another worker's
        // unrelated shared-state access while its Lua callback executes.
        methods.add_method("update", |lua, this, (key, func): (String, mlua::Function)| {
            for _ in 0..MAX_UPDATE_RETRIES {
                let (current, version) = this.shared.read_for_update(&key);
                let current_lua = shared_value_to_lua(lua, &current)?;
                let new_lua: Value = func.call(current_lua)?;
                let new_shared = match lua_value_to_shared(&new_lua) {
                    Ok(v) => v,
                    Err(e) => {
                        return crate::lua::api::errors::err_pair(
                            lua,
                            crate::lua::error::ScriptError::new("invalid_configuration", e.to_string()),
                        )
                    }
                };
                match this.shared.compare_and_swap(&key, version, new_shared.clone()) {
                    Ok(true) => return Ok((shared_value_to_lua(lua, &new_shared)?, Value::Nil)),
                    Ok(false) => continue,
                    Err(e) => {
                        return crate::lua::api::errors::err_pair(
                            lua,
                            crate::lua::error::ScriptError::new("invalid_configuration", e.to_string()),
                        )
                    }
                }
            }
            crate::lua::api::errors::err_pair(
                lua,
                crate::lua::error::ScriptError::new("invalid_configuration", "shared:update contention exceeded retry limit")
                    .retryable(true),
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrips() {
        let state = SharedState::new();
        state.set("k".to_string(), SharedValue::Number(42.0)).unwrap();
        assert_eq!(state.get("k"), Some(SharedValue::Number(42.0)));
    }

    #[test]
    fn compare_and_swap_fails_on_stale_version() {
        let state = SharedState::new();
        state.set("k".to_string(), SharedValue::Number(1.0)).unwrap();
        let (_, version) = state.read_for_update("k");
        // A concurrent write bumps the version.
        state.set("k".to_string(), SharedValue::Number(2.0)).unwrap();
        let ok = state
            .compare_and_swap("k", version, SharedValue::Number(3.0))
            .unwrap();
        assert!(!ok, "stale version must be rejected");
        assert_eq!(state.get("k"), Some(SharedValue::Number(2.0)));
    }

    #[test]
    fn compare_and_swap_succeeds_on_current_version() {
        let state = SharedState::new();
        state.set("k".to_string(), SharedValue::Number(1.0)).unwrap();
        let (_, version) = state.read_for_update("k");
        let ok = state
            .compare_and_swap("k", version, SharedValue::Number(2.0))
            .unwrap();
        assert!(ok);
        assert_eq!(state.get("k"), Some(SharedValue::Number(2.0)));
    }

    #[test]
    fn store_full_is_rejected() {
        let state = SharedState::new();
        for i in 0..MAX_STORE_KEYS {
            state.set(format!("k{i}"), SharedValue::Bool(true)).unwrap();
        }
        let err = state.set("overflow".to_string(), SharedValue::Bool(true)).unwrap_err();
        assert_eq!(err, SharedStateError::StoreFull);
    }
}
