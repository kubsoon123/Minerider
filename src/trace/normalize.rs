//! Trace normalization: replaces session-specific values with stable
//! symbols so captures become deterministic, comparable fixtures.
//!
//! Rules preserve semantic relationships: the same raw value always maps
//! to the same symbol within a trace (e.g. a teleport id appearing in both
//! `synchronize_player_position` and `accept_teleportation` becomes
//! `TELEPORT_ID_1` in both places).
//!
//! Normalization also redacts: absolute timestamps and raw payloads are
//! dropped (payloads contain the un-normalized bytes of ids and UUIDs).

use std::collections::HashMap;

use serde_json::Value;

use super::format::TraceEvent;

/// Maps raw values of one kind to stable symbols (`UUID_1`, `UUID_2`, …).
struct SymbolTable {
    maps: HashMap<&'static str, HashMap<String, String>>,
    counters: HashMap<&'static str, usize>,
}

impl SymbolTable {
    fn new() -> Self {
        Self {
            maps: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    fn symbol(&mut self, kind: &'static str, raw: &Value) -> String {
        let key = match raw {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let map = self.maps.entry(kind).or_default();
        if let Some(sym) = map.get(&key) {
            return sym.clone();
        }
        let counter = self.counters.entry(kind).or_insert(0);
        *counter += 1;
        let sym = format!("{kind}_{counter}");
        map.insert(key, sym.clone());
        sym
    }
}

/// Classifies a field key into a symbol kind, if session-specific.
fn symbol_kind(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_lowercase();
    if key.contains("uuid") {
        Some("UUID")
    } else if key == "teleport_id" {
        Some("TELEPORT_ID")
    } else if key == "entity_id" || key == "player_id" || key.ends_with("_entity_id") {
        Some("ENTITY_ID")
    } else if key.contains("timestamp") {
        Some("TIMESTAMP")
    } else if key == "salt" {
        Some("SALT")
    } else if key == "sequence" || key == "sequence_number" {
        Some("SEQUENCE")
    } else {
        None
    }
}

fn normalize_value(value: &mut Value, symbols: &mut SymbolTable) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if let Some(kind) = symbol_kind(key) {
                    *child = Value::String(symbols.symbol(kind, child));
                } else {
                    normalize_value(child, symbols);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_value(item, symbols);
            }
        }
        _ => {}
    }
}

/// Normalizes one event in place using the shared symbol tables.
fn normalize_event(event: &mut TraceEvent, symbols: &mut SymbolTable) {
    event.session = symbols.symbol("SESSION", &Value::String(event.session.clone()));
    // Absolute wall-clock time is never comparable; relative timing is
    // what the diff engine checks (with tolerance classes).
    event.ts_mono_ms = 0.0;
    // Raw payloads embed the un-normalized ids; drop them in fixtures.
    event.payload_hex.clear();
    if let Some(fields) = &mut event.fields {
        // Keep-alive ids are correlated between directions; give them a
        // dedicated symbol kind so echoes map to the same symbol.
        if event.name == "keep_alive" {
            if let Some(id) = fields.get_mut("id") {
                *id = Value::String(symbols.symbol("KEEPALIVE_ID", id));
            }
        }
        normalize_value(fields, symbols);
    }
}

/// Normalizes a whole trace. Input order is preserved.
pub fn normalize(events: &[TraceEvent]) -> Vec<TraceEvent> {
    let mut symbols = SymbolTable::new();
    events
        .iter()
        .cloned()
        .map(|mut event| {
            normalize_event(&mut event, &mut symbols);
            event
        })
        .collect()
}
