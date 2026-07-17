//! Login state: login start, encryption exchange, compression, success.
//!
//! Packet ids and layouts come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::login`); only the flow
//! (ordering, crypto, compression handoff) is hand-written.
//!
//! Online-mode Mojang session authentication is optional: pass `premium` to
//! identify with a real Microsoft account (see [`crate::auth`]) and join the
//! session server before answering the encryption request, exactly as
//! vanilla does. Without it, clients log in as offline-mode users (all-zero
//! UUID); the shared secret and verify token are still exchanged and AES
//! encryption is still enabled, which offline-mode servers also require.

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

use crate::auth::PremiumSession;
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
/// With `premium: None`, identifies as offline-mode (all-zero UUID) and
/// never contacts Mojang session servers. With `Some(session)`, sends the
/// real profile identity and — if the server reports
/// `should_authenticate` — calls the session server's `join` endpoint after
/// computing the shared secret but before answering the encryption request,
/// matching the order a real online-mode login requires.
pub async fn login(
    conn: &mut Connection,
    username: &str,
    premium: Option<&PremiumSession>,
) -> Result<LoginSuccess> {
    let start = match premium {
        Some(session) => PacketLoginStart {
            username: session.username.clone(),
            player_uuid: session.uuid,
        },
        None => {
            // Vanilla validates the username client-side before ever opening
            // a connection; without this, an invalid name reaches the server
            // as a raw string and typically comes back as an opaque Netty
            // decode-exception disconnect instead of a clear local error.
            if !is_valid_username(username) {
                return Err(MineRiderError::Protocol(format!(
                    "invalid username {username:?}: must be 3-16 characters, letters/digits/underscore only"
                )));
            }
            PacketLoginStart {
                username: username.to_string(),
                // Offline mode: UUID is all zeros.
                player_uuid: 0,
            }
        }
    };
    let sent_username = start.username.clone();
    let mut w = PacketWriter::new();
    start.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_LOGIN_START_ID, &w.freeze())
        .await?;
    info!(username = %sent_username, premium = premium.is_some(), "sent login start");

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
                handle_encryption_request(conn, &request, premium).await?;
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

/// Vanilla's offline/legacy username rule: 3-16 ASCII letters, digits or
/// underscores. Real Microsoft-account usernames can be validated the same
/// way and always pass, so this only ever rejects offline-mode input.
fn is_valid_username(username: &str) -> bool {
    (3..=16).contains(&username.len())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn handle_encryption_request(
    conn: &mut Connection,
    request: &PacketEncryptionBegin,
    premium: Option<&PremiumSession>,
) -> Result<()> {
    let mut shared_secret = [0u8; 16];
    OsRng.fill_bytes(&mut shared_secret);

    // The session server join must happen with the shared secret in hand but
    // strictly before the encryption response is sent: the server may call
    // Mojang's `hasJoined` as soon as it receives (and decrypts) that
    // response, and a `hasJoined` that races ahead of our `join` fails.
    //
    // This HTTP call has no timeout of its own: it runs inside `login()`,
    // which runs inside `Client::connect`'s single overall connect-to-play
    // deadline (`ClientConfig::connect_deadline`), so it's already bounded —
    // adding a second, independent timeout here would just be two clocks
    // racing for no benefit. (The device-code Microsoft sign-in flow that
    // produces a `PremiumSession` in the first place is a separate,
    // human-paced operation that happens before `Client::connect` is even
    // called, and already has its own deadline: the device code's own
    // `expires_in`, honored by `auth::live::poll_for_token`.)
    if request.should_authenticate {
        match premium {
            Some(session) => {
                session
                    .join_session(&request.server_id, &shared_secret, &request.public_key)
                    .await?;
                debug!("joined session server");
            }
            None => {
                return Err(MineRiderError::Protocol(
                    "server requires online-mode authentication but no premium session was supplied"
                        .to_string(),
                ));
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_vanilla_shaped_usernames() {
        assert!(is_valid_username("Notch"));
        assert!(is_valid_username("a_b_c"));
        assert!(is_valid_username("abc")); // exactly 3, the minimum
        assert!(is_valid_username(&"a".repeat(16))); // exactly 16, the maximum
    }

    #[test]
    fn rejects_out_of_range_length() {
        assert!(!is_valid_username("ab")); // 2 chars, below minimum
        assert!(!is_valid_username(&"a".repeat(17))); // 17 chars, above maximum
        assert!(!is_valid_username(""));
    }

    #[test]
    fn rejects_disallowed_characters() {
        assert!(!is_valid_username("bad name")); // space
        assert!(!is_valid_username("bad-name")); // hyphen
        assert!(!is_valid_username("bäd")); // non-ASCII
    }

    // ------------------------------------------------------------------
    // Live wiring: premium login must join the session server after the
    // shared secret is known but *before* the encryption response is sent.
    // A mock Minecraft login server plus a hand-rolled mock HTTP session
    // server (matching this project's existing no-framework-mocking
    // convention in tests/common) drive `login()` for real over a TCP
    // socket, so this exercises the exact code path a live server would.
    // ------------------------------------------------------------------

    use ::rsa::pkcs8::EncodePublicKey;
    use ::rsa::Pkcs1v15Encrypt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::auth::{server_id_hash, PremiumSession};
    use crate::core::state::ConnectionState;
    use minerider_protocol::crypto::rsa as mc_rsa;
    use minerider_protocol::generated::v1_21_4::handshaking;
    use minerider_protocol::generated::versions::V1_21_4;

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A minimal hand-rolled HTTP/1.1 server that accepts one connection,
    /// captures its request body, and replies `204 No Content`.
    async fn mock_http_capture_one(listener: TcpListener) -> Vec<u8> {
        let (mut stream, _) = listener.accept().await.expect("accept mock http");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let headers_end = loop {
            let n = stream.read(&mut chunk).await.expect("read mock http");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let header_text = String::from_utf8_lossy(&buf[..headers_end]).to_lowercase();
        let content_length: usize = header_text
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse().expect("content-length is a number"))
            .unwrap_or(0);
        while buf.len() < headers_end + content_length {
            let n = stream.read(&mut chunk).await.expect("read mock http body");
            buf.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .expect("write mock http response");
        buf[headers_end..headers_end + content_length].to_vec()
    }

    #[tokio::test]
    async fn premium_login_joins_session_server_before_encryption_response() {
        let http_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock session server");
        let http_port = http_listener.local_addr().unwrap().port();
        let http_task = tokio::spawn(mock_http_capture_one(http_listener));

        let mc_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock minecraft server");
        let mc_port = mc_listener.local_addr().unwrap().port();

        let uuid: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;
        let access_token = "test-mc-access-token".to_string();

        let mc_task = tokio::spawn(async move {
            let (stream, _) = mc_listener.accept().await.expect("accept mock mc");
            let mut conn = Connection::from_tcp_stream(stream).expect("wrap mock mc stream");

            let hs = conn.read_packet().await.expect("read handshake");
            assert_eq!(hs.id, handshaking::SERVERBOUND_SET_PROTOCOL_ID);
            let login_start = conn.read_packet().await.expect("read login start");
            assert_eq!(login_start.id, SERVERBOUND_LOGIN_START_ID);

            let (public, private) = mc_rsa::generate_keypair(1024).expect("keypair");
            let der = public.to_public_key_der().expect("der encode");
            let verify_token = [0x11u8, 0x22, 0x33, 0x44];
            let server_id = String::new();

            let mut w = PacketWriter::new();
            w.put_string(&server_id).unwrap();
            w.put_byte_array(der.as_bytes());
            w.put_byte_array(&verify_token);
            w.put_bool(true); // should_authenticate: this is an online-mode server
            conn.send_packet(CLIENTBOUND_ENCRYPTION_BEGIN_ID, &w.into_inner())
                .await
                .expect("send encryption_begin");

            let resp = conn.read_packet().await.expect("read encryption response");
            assert_eq!(resp.id, SERVERBOUND_ENCRYPTION_BEGIN_ID);
            let (encrypted_secret, encrypted_token) = {
                let mut r = PacketReader::new(&resp.payload);
                (
                    r.read_byte_array().unwrap().to_vec(),
                    r.read_byte_array().unwrap().to_vec(),
                )
            };
            let shared_secret = private
                .decrypt(Pkcs1v15Encrypt, &encrypted_secret)
                .expect("decrypt shared secret");
            let token = private
                .decrypt(Pkcs1v15Encrypt, &encrypted_token)
                .expect("decrypt verify token");
            assert_eq!(token, verify_token, "verify token must round-trip");
            let secret_array: [u8; 16] = shared_secret.clone().try_into().unwrap();
            conn.enable_encryption(&secret_array);

            // Complete the login so the client-side call returns Ok.
            let mut w = PacketWriter::new();
            w.put_varint(64);
            conn.send_packet(CLIENTBOUND_COMPRESS_ID, &w.into_inner())
                .await
                .expect("send compress");
            conn.set_compression(64);
            let mut w = PacketWriter::new();
            w.put_uuid(uuid);
            w.put_string("PremiumTestUser").unwrap();
            w.put_varint(0);
            conn.send_packet(CLIENTBOUND_SUCCESS_ID, &w.into_inner())
                .await
                .expect("send success");

            (shared_secret, der.as_bytes().to_vec(), server_id)
        });

        let premium = PremiumSession::for_test(
            access_token.clone(),
            uuid,
            "PremiumTestUser",
            format!("http://127.0.0.1:{http_port}/join"),
        );

        let mut client_conn = Connection::connect("127.0.0.1", mc_port)
            .await
            .expect("client connect to mock mc server");
        crate::minecraft::handshake::send(&mut client_conn, V1_21_4.protocol, "127.0.0.1", mc_port)
            .await
            .expect("send handshake");
        client_conn.set_state(ConnectionState::Login);

        let success = login(&mut client_conn, "unused-offline-username", Some(&premium))
            .await
            .expect("premium login should succeed");
        assert_eq!(success.username, "PremiumTestUser");

        let (shared_secret, public_key_der, server_id) = mc_task.await.expect("mc task");
        let join_body = http_task.await.expect("http task");

        // The join call must have actually happened, with the right fields,
        // by the time login() returns — proving both that it ran (this test
        // hangs/fails otherwise, since the mock mc server won't get its
        // encryption response validated without our code calling join first)
        // and that the values it sent are correct.
        let join_json: serde_json::Value =
            serde_json::from_slice(&join_body).expect("join body is JSON");
        assert_eq!(join_json["accessToken"], access_token);
        assert_eq!(join_json["selectedProfile"], format!("{uuid:032x}"));
        let expected_hash = server_id_hash(&server_id, &shared_secret, &public_key_der);
        assert_eq!(join_json["serverId"], expected_hash);
    }
}
