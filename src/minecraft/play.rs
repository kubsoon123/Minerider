//! Play state entry point: keep-alive handling. Full behavior in phase 3.

use minerider_protocol::buffer::PacketReader;
use tracing::debug;

use crate::core::error::{MineRiderError, Result};
use crate::network::connection::Connection;

/// Clientbound play: Keep Alive (1.21.4, payload: i64).
pub const CLIENTBOUND_KEEP_ALIVE: i32 = 0x26;
/// Serverbound play: Keep Alive (1.21.4, payload: i64).
pub const SERVERBOUND_KEEP_ALIVE: i32 = 0x18;
/// Clientbound play: Disconnect (1.21.4, payload: JSON string reason).
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
                let mut r = PacketReader::new(&packet.payload);
                let reason = r.read_string()?;
                return Err(MineRiderError::Disconnected(reason.to_string()));
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
