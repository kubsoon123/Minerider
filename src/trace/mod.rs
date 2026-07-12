//! Packet trace capture: a deterministic JSONL format recording every
//! packet in both directions, for vanilla-fidelity conformance work.
//!
//! One line per event:
//!
//! ```json
//! {"ts_mono_ms":12.3,"ts_rel_ms":12.3,"tick":0,"dir":"serverbound",
//!  "state":"login","id":0,"name":"login_start","fields":{...},
//!  "payload_hex":"...","encrypted":false,"compressed":false,"size":21,
//!  "session":"SESSION_1","scenario":"offline_login","step":1}
//! ```
//!
//! Raw captures may contain session-specific values; anything committed as
//! a fixture must pass through [`normalize`](crate::trace::normalize)
//! first, which also redacts/normalizes UUIDs, ids, tokens and addresses.

pub mod decode;
pub mod format;
pub mod recorder;

pub use format::{Direction, TraceEvent};
pub use recorder::TraceRecorder;
