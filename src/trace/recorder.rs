//! The trace recorder: writes one JSONL event per packet, flushed
//! immediately so a crash never loses the divergence point.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use crate::core::state::ConnectionState;
use crate::trace::decode::{decode_fields, packet_name, state_name};
use crate::trace::format::{hex, Direction, TraceEvent};

/// Records packet events to a JSONL trace file.
pub struct TraceRecorder {
    out: BufWriter<File>,
    start: Instant,
    session: String,
    scenario: String,
    step: u32,
}

impl TraceRecorder {
    /// Creates a recorder writing to `path`.
    ///
    /// `session` must already be a normalized identifier (e.g.
    /// `SESSION_1`) — never a real session id, token or server address.
    pub fn create(
        path: impl AsRef<Path>,
        session: impl Into<String>,
        scenario: impl Into<String>,
    ) -> std::io::Result<TraceRecorder> {
        Ok(TraceRecorder {
            out: BufWriter::new(File::create(path)?),
            start: Instant::now(),
            session: session.into(),
            scenario: scenario.into(),
            step: 0,
        })
    }

    /// Advances the scenario step recorded on subsequent events.
    pub fn set_step(&mut self, step: u32) {
        self.step = step;
    }

    /// Switches the scenario name recorded on subsequent events.
    pub fn set_scenario(&mut self, scenario: impl Into<String>) {
        self.scenario = scenario.into();
        self.step = 0;
    }

    /// Records one packet event.
    pub(crate) fn record(
        &mut self,
        dir: Direction,
        state: ConnectionState,
        id: i32,
        payload: &[u8],
        encrypted: bool,
        compressed: bool,
    ) {
        let ts_rel_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        let name = packet_name(state, dir, id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("unknown_0x{id:02x}"));
        let event = TraceEvent {
            ts_mono_ms: ts_rel_ms,
            ts_rel_ms,
            tick: (ts_rel_ms / 50.0) as u64,
            dir,
            state: state_name(state).to_string(),
            id,
            name,
            fields: decode_fields(state, dir, id, payload),
            payload_hex: hex(payload),
            encrypted,
            compressed,
            size: minerider_protocol::varint::varint_size(id) + payload.len(),
            session: self.session.clone(),
            scenario: self.scenario.clone(),
            step: self.step,
        };
        // A trace write failure must never break the connection; tracing is
        // observational only.
        if serde_json::to_writer(&mut self.out, &event).is_ok() {
            let _ = self.out.write_all(b"\n");
            let _ = self.out.flush();
        }
    }
}

/// Reads a JSONL trace file back into events.
pub fn read_trace(path: impl AsRef<Path>) -> std::io::Result<Vec<TraceEvent>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}
