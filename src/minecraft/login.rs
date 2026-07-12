//! Login state: login start, encryption exchange, compression, success.
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

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::network::connection::Connection;

/// Clientbound login: Disconnect (0x00).
pub const CLIENTBOUND_DISCONNECT: i32 = 0x00;
/// Clientbound login: Encryption Request (0x01).
pub const CLIENTBOUND_ENCRYPTION_REQUEST: i32 = 0x01;
/// Clientbound login: Login Success (0x02).
pub const CLIENTBOUND_LOGIN_SUCCESS: i32 = 0x02;
/// Clientbound login: Set Compression (0x03).
pub const CLIENTBOUND_SET_COMPRESSION: i32 = 0x03;
/// Clientbound login: Login Plugin Request (0x04).
pub const CLIENTBOUND_LOGIN_PLUGIN_REQUEST: i32 = 0x04;

/// Serverbound login: Login Start (0x00).
pub const SERVERBOUND_LOGIN_START: i32 = 0x00;
/// Serverbound login: Encryption Response (0x01).
pub const SERVERBOUND_ENCRYPTION_RESPONSE: i32 = 0x01;
/// Serverbound login: Login Plugin Response (0x02).
pub const SERVERBOUND_LOGIN_PLUGIN_RESPONSE: i32 = 0x02;
/// Serverbound login: Login Acknowledged (0x03).
pub const SERVERBOUND_LOGIN_ACKNOWLEDGED: i32 = 0x03;

/// Safety bound on packets read during login.
const MAX_LOGIN_PACKETS: usize = 64;

/// Clientbound Encryption Request (0x01).
#[derive(Debug, Clone)]
pub struct EncryptionRequest {
    /// Server id (empty in modern versions).
    pub server_id: String,
    /// DER-encoded SubjectPublicKeyInfo of the server's RSA key.
    pub public_key_der: Vec<u8>,
    /// Token the client must echo back encrypted.
    pub verify_token: Vec<u8>,
    /// Whether the server expects Mojang session authentication.
    pub should_authenticate: bool,
}

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
    let mut start = PacketWriter::new();
    start.put_string(username)?;
    // Offline mode: UUID is all zeros. Online-mode session join is not
    // implemented yet.
    start.put_uuid(0);
    conn.send_packet(SERVERBOUND_LOGIN_START, &start.into_inner())
        .await?;
    info!(%username, "sent login start");

    for _ in 0..MAX_LOGIN_PACKETS {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_DISCONNECT => {
                let mut r = PacketReader::new(&packet.payload);
                let reason = r.read_string()?;
                return Err(MineRiderError::Disconnected(reason.to_string()));
            }
            CLIENTBOUND_ENCRYPTION_REQUEST => {
                let request = parse_encryption_request(&packet.payload)?;
                handle_encryption_request(conn, &request).await?;
            }
            CLIENTBOUND_LOGIN_SUCCESS => {
                let success = parse_login_success(&packet.payload)?;
                conn.send_packet(SERVERBOUND_LOGIN_ACKNOWLEDGED, &[])
                    .await?;
                conn.set_state(ConnectionState::Configuration);
                info!(uuid = %success.uuid, username = %success.username, "login success");
                return Ok(success);
            }
            CLIENTBOUND_SET_COMPRESSION => {
                let mut r = PacketReader::new(&packet.payload);
                let threshold = r.get_varint()?;
                debug!(threshold, "set compression");
                conn.set_compression(threshold);
            }
            CLIENTBOUND_LOGIN_PLUGIN_REQUEST => {
                let mut r = PacketReader::new(&packet.payload);
                let message_id = r.get_varint()?;
                let channel = r.read_string()?;
                debug!(message_id, %channel, "login plugin request (unsupported)");
                // Empty data = not understood by the client.
                let mut w = PacketWriter::new();
                w.put_varint(message_id);
                w.put_bool(false);
                conn.send_packet(SERVERBOUND_LOGIN_PLUGIN_RESPONSE, &w.into_inner())
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

fn parse_encryption_request(payload: &[u8]) -> Result<EncryptionRequest> {
    let mut r = PacketReader::new(payload);
    let request = EncryptionRequest {
        server_id: r.read_string()?.to_string(),
        public_key_der: r.read_byte_array()?.to_vec(),
        verify_token: r.read_byte_array()?.to_vec(),
        should_authenticate: r.get_bool()?,
    };
    Ok(request)
}

async fn handle_encryption_request(
    conn: &mut Connection,
    request: &EncryptionRequest,
) -> Result<()> {
    let mut shared_secret = [0u8; 16];
    OsRng.fill_bytes(&mut shared_secret);

    let encrypted_secret = rsa::encrypt_pkcs1v15(&request.public_key_der, &shared_secret)?;
    let encrypted_token = rsa::encrypt_pkcs1v15(&request.public_key_der, &request.verify_token)?;

    let mut w = PacketWriter::new();
    w.put_byte_array(&encrypted_secret);
    w.put_byte_array(&encrypted_token);
    conn.send_packet(SERVERBOUND_ENCRYPTION_RESPONSE, &w.into_inner())
        .await?;

    // From this byte on, everything in both directions is AES-CFB8 encrypted.
    conn.enable_encryption(&shared_secret);
    debug!(
        should_authenticate = request.should_authenticate,
        "encryption enabled"
    );
    Ok(())
}

fn parse_login_success(payload: &[u8]) -> Result<LoginSuccess> {
    let mut r = PacketReader::new(payload);
    let uuid = r.read_uuid()?;
    let username = r.read_string()?.to_string();
    let property_count = r.get_varint()?;
    if property_count < 0 {
        return Err(MineRiderError::Protocol(format!(
            "negative property count {property_count}"
        )));
    }
    for _ in 0..property_count {
        let _name = r.read_string()?;
        let _value = r.read_string()?;
        if r.get_bool()? {
            let _signature = r.read_string()?;
        }
    }
    Ok(LoginSuccess { uuid, username })
}
