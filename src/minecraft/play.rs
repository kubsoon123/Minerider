//! Play state entry point: keep-alive handling. Full behavior in phase 3.
//!
//! Packet ids and layouts come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::play`).

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::play::{
    PacketKeepAlive, PacketKickDisconnect, CLIENTBOUND_KEEP_ALIVE_ID,
    CLIENTBOUND_KICK_DISCONNECT_ID, SERVERBOUND_KEEP_ALIVE_ID,
};
use minerider_protocol::traits::{Decode, Encode};
use tracing::debug;

use crate::core::error::{MineRiderError, Result};
use crate::minecraft::nbt_reason_text;
use crate::network::connection::Connection;

/// Runs the minimal play-state loop: answers keep-alives, reports
/// disconnects, ignores everything else. Returns only on error/disconnect.
pub async fn run_play(conn: &mut Connection) -> Result<()> {
    loop {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_KEEP_ALIVE_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let keep_alive = PacketKeepAlive::decode(&mut r)?;
                let mut w = PacketWriter::new();
                keep_alive.encode(&mut w)?;
                conn.send_packet(SERVERBOUND_KEEP_ALIVE_ID, &w.freeze())
                    .await?;
            }
            CLIENTBOUND_KICK_DISCONNECT_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let disconnect = PacketKickDisconnect::decode(&mut r)?;
                return Err(MineRiderError::Disconnected(nbt_reason_text(
                    &disconnect.reason,
                )));
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
