//! Login state: login start, encryption exchange, compression, success.
//!
//! Packet ids and layouts come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::login`); only the flow
//! (ordering, crypto, compression handoff) is hand-written.
//!
//! Online-mode Mojang session authentication (`should_authenticate`) is not
//! implemented yet; clients are logged in as offline-mode users. The shared
//! secret and verify token are still exchanged and AES encryption is still
//! enabled, which offline-mode servers require.

use rand::rngs::OsRng;
use rand::RngCore;
use tracing::{debug, info};

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::crypto::rsa;
use minerider_protocol::generated::v1_21_4::login::{
    PacketCompress, PacketDisconnect, PacketEncryptionBegin, PacketEncryptionBeginServerbound,
    PacketLoginPluginRequest, PacketLoginPluginResponse, PacketLoginStart, PacketSuccess,
    CLIENTBOUND_COMPRESS_ID, CLIENTBOUND_DISCONNECT_ID, CLIENTBOUND_ENCRYPTION_BEGIN_ID,
    CLIENTBOUND_LOGIN_PLUGIN_REQUEST_ID, CLIENTBOUND_SUCCESS_ID, SERVERBOUND_ENCRYPTION_BEGIN_ID,
    SERVERBOUND_LOGIN_ACKNOWLEDGED_ID, SERVERBOUND_LOGIN_PLUGIN_RESPONSE_ID,
    SERVERBOUND_LOGIN_START_ID,
};
use minerider_protocol::traits::{Decode, Encode};

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::network::connection::Connection;

/// Safety bound on packets read during login.
const MAX_LOGIN_PACKETS: usize = 64;

/// Result of a successful login: the identity the server assigned.
#[derive(Debug, Clone)]
pub struct LoginSuccess {
    /// UUID assigned by the server.
    pub uuid: u128,
    /// Username confirmed by the server.
    pub username: String,
}

/// Runs the login state: sends Login Start and processes the login flow
/// until Login Success, leaving the connection in
/// [`ConnectionState::Configuration`].
///
/// The client identifies as offline-mode (all-zero UUID) and never contacts
/// Mojang session servers; see the module docs.
pub async fn login(conn: &mut Connection, username: &str) -> Result<LoginSuccess> {
    let start = PacketLoginStart {
        username: username.to_string(),
        // Offline mode: UUID is all zeros. Online-mode session join is not
        // implemented yet.
        player_uuid: 0,
    };
    let mut w = PacketWriter::new();
    start.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_LOGIN_START_ID, &w.freeze())
        .await?;
    info!(%username, "sent login start");

    for _ in 0..MAX_LOGIN_PACKETS {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_DISCONNECT_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let disconnect = PacketDisconnect::decode(&mut r)?;
                return Err(MineRiderError::Disconnected(disconnect.reason));
            }
            CLIENTBOUND_ENCRYPTION_BEGIN_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let request = PacketEncryptionBegin::decode(&mut r)?;
                handle_encryption_request(conn, &request).await?;
            }
            CLIENTBOUND_SUCCESS_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let success = PacketSuccess::decode(&mut r)?;
                conn.send_packet(SERVERBOUND_LOGIN_ACKNOWLEDGED_ID, &[])
                    .await?;
                conn.set_state(ConnectionState::Configuration);
                info!(uuid = %success.uuid, username = %success.username, "login success");
                return Ok(LoginSuccess {
                    uuid: success.uuid,
                    username: success.username,
                });
            }
            CLIENTBOUND_COMPRESS_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let compress = PacketCompress::decode(&mut r)?;
                debug!(threshold = compress.threshold, "set compression");
                conn.set_compression(compress.threshold);
            }
            CLIENTBOUND_LOGIN_PLUGIN_REQUEST_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let request = PacketLoginPluginRequest::decode(&mut r)?;
                debug!(
                    message_id = request.message_id,
                    %request.channel,
                    "login plugin request (unsupported)"
                );
                // `data: None` = not understood by the client.
                let response = PacketLoginPluginResponse {
                    message_id: request.message_id,
                    data: None,
                };
                let mut w = PacketWriter::new();
                response.encode(&mut w)?;
                conn.send_packet(SERVERBOUND_LOGIN_PLUGIN_RESPONSE_ID, &w.freeze())
                    .await?;
            }
            other => {
                return Err(MineRiderError::Protocol(format!(
                    "unexpected clientbound login packet id 0x{other:02x}"
                )));
            }
        }
    }

    Err(MineRiderError::Protocol(
        "login did not complete within 64 packets".to_string(),
    ))
}

async fn handle_encryption_request(
    conn: &mut Connection,
    request: &PacketEncryptionBegin,
) -> Result<()> {
    let mut shared_secret = [0u8; 16];
    OsRng.fill_bytes(&mut shared_secret);

    let encrypted_secret = rsa::encrypt_pkcs1v15(&request.public_key, &shared_secret)?;
    let encrypted_token = rsa::encrypt_pkcs1v15(&request.public_key, &request.verify_token)?;

    let response = PacketEncryptionBeginServerbound {
        shared_secret: encrypted_secret,
        verify_token: encrypted_token,
    };
    let mut w = PacketWriter::new();
    response.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_ENCRYPTION_BEGIN_ID, &w.freeze())
        .await?;

    // From this byte on, everything in both directions is AES-CFB8 encrypted.
    conn.enable_encryption(&shared_secret);
    debug!(
        should_authenticate = request.should_authenticate,
        "encryption enabled"
    );
    Ok(())
}
