//! Play state entry point: keep-alive handling. Full behavior in phase 3.

use minerider_protocol::buffer::PacketReader;
use tracing::debug;

use crate::core::error::{MineRiderError, Result};
use crate::network::connection::Connection;

/// Clientbound play: Keep Alive (1.21.4, payload: i64).
pub const CLIENTBOUND_KEEP_ALIVE: i32 = 0x27;
/// Serverbound play: Keep Alive (1.21.4, payload: i64).
pub const SERVERBOUND_KEEP_ALIVE: i32 = 0x1a;
/// Clientbound play: Disconnect (1.21.4, payload: NBT text component reason).
pub const CLIENTBOUND_DISCONNECT: i32 = 0x1d;

// NOTE: these packet ids are hard-coded for phase 1. From phase 2 on they
// come from the registry generated out of minecraft-data.

/// Runs the minimal play-state loop: answers keep-alives, reports
/// disconnects, ignores everything else. Returns only on error/disconnect.
pub async fn run_play(conn: &mut Connection) -> Result<()> {
    loop {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_KEEP_ALIVE => {
                // Echo the same i64 payload back.
                conn.send_packet(SERVERBOUND_KEEP_ALIVE, &packet.payload)
                    .await?;
            }
            CLIENTBOUND_DISCONNECT => {
                let r = PacketReader::new(&packet.payload);
                // The reason is a network NBT text component (anonymousNbt).
                // We keep the raw NBT payload as lossy UTF-8 — the component
                // text stays readable; proper decoding lands with the
                // generated protocol in Phase 2.
                let reason = String::from_utf8_lossy(r.rest()).into_owned();
                return Err(MineRiderError::Disconnected(reason));
            }
            other => {
                debug!(
                    id = format_args!("0x{other:02x}"),
                    len = packet.payload.len(),
                    "ignoring play packet"
                );
            }
        }
    }
}
