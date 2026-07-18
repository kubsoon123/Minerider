//! `SharedValue`: the bounded, Lua-compatible value type that crosses the
//! cross-worker shared-state (`swarm.shared`) and pub/sub
//! (`swarm:publish`/`on_message`) boundaries. Deliberately not `mlua`-aware
//! — this is plain Rust data so it can be cloned freely across worker
//! threads (each worker gets its own deep copy, never a shared reference
//! into another worker's Lua VM).
//!
//! Only nil/bool/finite-number/bounded-string/bounded-array/bounded
//! string-keyed-table values are representable — no functions, userdata,
//! threads, or non-finite numbers. Cycles are structurally impossible
//! (this is a tree, not a graph), and conversion from Lua enforces a
//! maximum nesting depth so a self-referential Lua table is rejected
//! rather than looping forever.

/// Maximum nesting depth for arrays/maps.
pub const MAX_DEPTH: usize = 8;
/// Maximum bytes for one string value.
pub const MAX_STRING_LEN: usize = 4096;
/// Maximum elements in one array.
pub const MAX_ARRAY_LEN: usize = 256;
/// Maximum keys in one map.
pub const MAX_MAP_KEYS: usize = 256;
/// Maximum total approximate serialized size of one value tree.
pub const MAX_TOTAL_SIZE: usize = 65_536;

#[derive(Debug, Clone, PartialEq)]
pub enum SharedValue {
    Nil,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<SharedValue>),
    /// Insertion-ordered string-keyed map (not a `HashMap`, so conversion
    /// back to Lua is deterministic for tests and diagnostics).
    Map(Vec<(String, SharedValue)>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SharedValueError {
    #[error("value nesting exceeds the maximum depth of {MAX_DEPTH}")]
    TooDeep,
    #[error("string exceeds the maximum length of {MAX_STRING_LEN} bytes")]
    StringTooLong,
    #[error("array exceeds the maximum length of {MAX_ARRAY_LEN}")]
    ArrayTooLong,
    #[error("table exceeds the maximum key count of {MAX_MAP_KEYS}")]
    TooManyKeys,
    #[error("value exceeds the maximum total serialized size of {MAX_TOTAL_SIZE} bytes")]
    TooLarge,
    #[error("value contains a non-finite number")]
    NonFiniteNumber,
    #[error("value contains an unsupported type: {0}")]
    UnsupportedType(&'static str),
    #[error("table keys must be strings or a dense 1-based integer sequence")]
    UnsupportedKey,
}

impl SharedValue {
    /// Approximate serialized size in bytes, used to enforce
    /// [`MAX_TOTAL_SIZE`] without actually serializing.
    pub fn approx_size(&self) -> usize {
        match self {
            SharedValue::Nil => 1,
            SharedValue::Bool(_) => 1,
            SharedValue::Number(_) => 8,
            SharedValue::Str(s) => s.len() + 8,
            SharedValue::Array(items) => {
                items.iter().map(SharedValue::approx_size).sum::<usize>() + 8
            }
            SharedValue::Map(entries) => {
                entries
                    .iter()
                    .map(|(k, v)| k.len() + v.approx_size() + 8)
                    .sum::<usize>()
                    + 8
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_size_grows_with_nesting() {
        let flat = SharedValue::Str("x".repeat(100));
        let nested = SharedValue::Array(vec![flat.clone(), flat.clone()]);
        assert!(nested.approx_size() > flat.approx_size());
    }
}
