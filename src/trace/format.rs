//! The trace event record (one JSONL line per packet).

use serde::{Deserialize, Serialize};

/// Packet direction relative to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Server → client.
    Clientbound,
    /// Client → server.
    Serverbound,
}

/// One captured packet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    /// Monotonic milliseconds (steady clock, not wall time).
    pub ts_mono_ms: f64,
    /// Milliseconds since connection start.
    pub ts_rel_ms: f64,
    /// Inferred client tick (ts_rel_ms / 50).
    pub tick: u64,
    /// Direction of the packet.
    pub dir: Direction,
    /// Protocol state at capture time.
    pub state: String,
    /// Packet id within `state`/`dir`.
    pub id: i32,
    /// Generated packet name (`unknown_0xNN` when unmapped).
    pub name: String,
    /// Decoded fields (best effort; null when decoding failed or the
    /// payload was not decoded).
    pub fields: Option<serde_json::Value>,
    /// Raw payload bytes (without the packet id), hex-encoded.
    pub payload_hex: String,
    /// Whether the connection was encrypted at capture time.
    pub encrypted: bool,
    /// Whether compression was enabled at capture time.
    pub compressed: bool,
    /// Framed packet size on the wire (id varint + payload).
    pub size: usize,
    /// Normalized session identifier (never a real session/token).
    pub session: String,
    /// Scenario this event belongs to.
    pub scenario: String,
    /// Step within the scenario.
    pub step: u32,
}

/// Hex-encodes bytes (lowercase, no separators).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decodes lowercase hex produced by [`hex`].
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}
