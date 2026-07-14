//! Configuration state: keep-alive/ping echo, known packs, finish.
//!
//! Packet ids come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::configuration`); the
//! disconnect reason is decoded with the generated NBT layout.

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::configuration::{
    PacketCustomPayload, PacketDisconnect, CLIENTBOUND_DISCONNECT_ID,
    CLIENTBOUND_FINISH_CONFIGURATION_ID, CLIENTBOUND_KEEP_ALIVE_ID, CLIENTBOUND_PING_ID,
    CLIENTBOUND_SELECT_KNOWN_PACKS_ID, SERVERBOUND_CUSTOM_PAYLOAD_ID,
    SERVERBOUND_FINISH_CONFIGURATION_ID, SERVERBOUND_KEEP_ALIVE_ID, SERVERBOUND_PONG_ID,
    SERVERBOUND_SELECT_KNOWN_PACKS_ID, SERVERBOUND_SETTINGS_ID,
};
use minerider_protocol::traits::{Decode, Encode};
use tracing::{debug, warn};

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::{brand_payload, nbt_reason_text, vanilla_client_information, BRAND_CHANNEL};
use crate::network::connection::Connection;

/// Safety bound on packets read during configuration.
const MAX_CONFIGURATION_PACKETS: usize = 512;

/// Sends the two packets a vanilla client emits on entering configuration:
/// `client_information` (settings) and the `minecraft:brand` plugin message.
async fn send_client_configuration(conn: &mut Connection) -> Result<()> {
    let mut w = PacketWriter::new();
    vanilla_client_information().encode(&mut w)?;
    conn.send_packet(SERVERBOUND_SETTINGS_ID, &w.freeze())
        .await?;

    let brand = PacketCustomPayload {
        channel: BRAND_CHANNEL.to_string(),
        data: brand_payload()?,
    };
    let mut w = PacketWriter::new();
    brand.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_CUSTOM_PAYLOAD_ID, &w.freeze())
        .await?;
    debug!("sent client_information and brand");
    Ok(())
}

/// Runs the configuration state until the server sends Finish
/// Configuration, leaving the connection in [`ConnectionState::Play`].
///
/// On entry the client sends its settings and brand, exactly as vanilla
/// does, before processing the server's configuration packets.
pub async fn run_configuration(conn: &mut Connection) -> Result<()> {
    send_client_configuration(conn).await?;

    for _ in 0..MAX_CONFIGURATION_PACKETS {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_DISCONNECT_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let disconnect = PacketDisconnect::decode(&mut r)?;
                return Err(MineRiderError::Disconnected(nbt_reason_text(
                    &disconnect.reason,
                )));
            }
            CLIENTBOUND_FINISH_CONFIGURATION_ID => {
                conn.send_packet(SERVERBOUND_FINISH_CONFIGURATION_ID, &[])
                    .await?;
                conn.set_state(ConnectionState::Play);
                debug!("configuration finished");
                return Ok(());
            }
            // Keep Alive, Ping and Select Known Packs are all answered by
            // echoing the payload back under the matching serverbound id.
            CLIENTBOUND_KEEP_ALIVE_ID | CLIENTBOUND_PING_ID | CLIENTBOUND_SELECT_KNOWN_PACKS_ID => {
                let response_id = match packet.id {
                    CLIENTBOUND_KEEP_ALIVE_ID => SERVERBOUND_KEEP_ALIVE_ID,
                    CLIENTBOUND_PING_ID => SERVERBOUND_PONG_ID,
                    // Claim to know every pack the server asks about; this is
                    // correct for vanilla clients that ship all vanilla packs.
                    _ => SERVERBOUND_SELECT_KNOWN_PACKS_ID,
                };
                conn.send_packet(response_id, &packet.payload).await?;
            }
            other => {
                let entry = clientbound_coverage(ConnectionState::Configuration, other);
                match entry.class {
                    CoverageClass::Handled | CoverageClass::IntentionallyIgnored => debug!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        coverage = ?entry.class,
                        "ignoring configuration packet"
                    ),
                    CoverageClass::StoredForLater | CoverageClass::Unsupported => warn!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        coverage = ?entry.class,
                        status = ?entry.obligation.status,
                        "configuration packet not handled"
                    ),
                }
            }
        }
    }

    Err(MineRiderError::Protocol(
        "configuration did not finish within 512 packets".to_string(),
    ))
}
