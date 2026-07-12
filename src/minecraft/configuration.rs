//! Configuration state: keep-alive/ping echo, known packs, finish.

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use tracing::debug;

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::network::connection::Connection;

/// Clientbound configuration: Disconnect (0x02).
pub const CLIENTBOUND_DISCONNECT: i32 = 0x02;
/// Clientbound configuration: Finish Configuration (0x03).
pub const CLIENTBOUND_FINISH_CONFIGURATION: i32 = 0x03;
/// Clientbound configuration: Keep Alive (0x04).
pub const CLIENTBOUND_KEEP_ALIVE: i32 = 0x04;
/// Clientbound configuration: Ping (0x05).
pub const CLIENTBOUND_PING: i32 = 0x05;
/// Clientbound configuration: Select Known Packs (0x0e).
pub const CLIENTBOUND_SELECT_KNOWN_PACKS: i32 = 0x0e;

/// Serverbound configuration: Finish Configuration (0x03).
pub const SERVERBOUND_FINISH_CONFIGURATION: i32 = 0x03;
/// Serverbound configuration: Keep Alive (0x04).
pub const SERVERBOUND_KEEP_ALIVE: i32 = 0x04;
/// Serverbound configuration: Pong (0x05).
pub const SERVERBOUND_PONG: i32 = 0x05;
/// Serverbound configuration: Select Known Packs (0x07).
pub const SERVERBOUND_SELECT_KNOWN_PACKS: i32 = 0x07;

/// Safety bound on packets read during configuration.
const MAX_CONFIGURATION_PACKETS: usize = 512;

/// Runs the configuration state until the server sends Finish
/// Configuration, leaving the connection in [`ConnectionState::Play`].
pub async fn run_configuration(conn: &mut Connection) -> Result<()> {
    for _ in 0..MAX_CONFIGURATION_PACKETS {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_DISCONNECT => {
                let mut r = PacketReader::new(&packet.payload);
                let reason = r.read_string()?;
                return Err(MineRiderError::Disconnected(reason.to_string()));
            }
            CLIENTBOUND_FINISH_CONFIGURATION => {
                conn.send_packet(SERVERBOUND_FINISH_CONFIGURATION, &[])
                    .await?;
                conn.set_state(ConnectionState::Play);
                debug!("configuration finished");
                return Ok(());
            }
            // Keep Alive, Ping and Select Known Packs are all answered by
            // echoing the payload back under the matching serverbound id.
            CLIENTBOUND_KEEP_ALIVE | CLIENTBOUND_PING | CLIENTBOUND_SELECT_KNOWN_PACKS => {
                let response_id = match packet.id {
                    CLIENTBOUND_KEEP_ALIVE => SERVERBOUND_KEEP_ALIVE,
                    CLIENTBOUND_PING => SERVERBOUND_PONG,
                    // Claim to know every pack the server asks about; this is
                    // correct for vanilla clients that ship all vanilla packs.
                    _ => SERVERBOUND_SELECT_KNOWN_PACKS,
                };
                conn.send_packet(response_id, &packet.payload).await?;
            }
            other => {
                debug!(
                    id = format_args!("0x{other:02x}"),
                    len = packet.payload.len(),
                    "ignoring configuration packet"
                );
            }
        }
    }

    Err(MineRiderError::Protocol(
        "configuration did not finish within 512 packets".to_string(),
    ))
}

/// Builds the payload of a Select Known Packs packet: a VarInt count
/// followed by (namespace, id, version) strings. Exposed for tests and the
/// mock server.
pub fn build_known_packs_payload(packs: &[(&str, &str, &str)]) -> Result<Vec<u8>> {
    let mut w = PacketWriter::new();
    w.put_varint(packs.len() as i32);
    for (namespace, id, version) in packs {
        w.put_string(namespace)?;
        w.put_string(id)?;
        w.put_string(version)?;
    }
    Ok(w.into_inner().to_vec())
}
