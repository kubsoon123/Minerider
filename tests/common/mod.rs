//! Shared test helpers: a mock Minecraft server that speaks the full
//! phase-1 flow using the same library primitives as the client.
//!
//! Compiled once per integration test binary; not every binary uses every
//! helper, so dead-code warnings are silenced here.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ::rsa::pkcs8::EncodePublicKey;
use ::rsa::Pkcs1v15Encrypt;
use minerider::core::error::{MineRiderError, Result};
use minerider::network::connection::Connection;
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::crypto::rsa;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Protocol version the mock server expects in the handshake.
pub const EXPECTED_PROTOCOL: i32 = 769;

/// A mock Minecraft server running the full connect flow on a background
/// task against `127.0.0.1:<port>`.
pub struct MockServer {
    /// Port the server is listening on.
    pub port: u16,
    /// Number of keep-alive echoes the server observed from the client.
    pub echo_count: Arc<AtomicUsize>,
    /// The keep-alive ids the client echoed back, in order.
    pub echo_ids: Arc<Mutex<Vec<i64>>>,
    handle: JoinHandle<Result<()>>,
}

impl MockServer {
    /// Starts a mock server that performs the full RSA + AES encryption
    /// handshake, then enables compression (threshold 64), and exchanges
    /// three play-state keep-alives before closing.
    pub async fn start_encrypted() -> MockServer {
        Self::start(Mode::Encrypted).await
    }

    /// Starts a mock server that skips the encryption exchange entirely and
    /// goes straight to compression (threshold 16, so the ≥ 16-byte
    /// LoginSuccess is compressed while 9-byte keep-alives stay raw), then
    /// exchanges one play-state keep-alive before closing.
    pub async fn start_plain() -> MockServer {
        Self::start(Mode::Plain).await
    }

    /// Starts a mock server that logs the client in without encryption,
    /// then kicks it during configuration with a Disconnect packet whose
    /// reason is a network NBT text component.
    pub async fn start_config_disconnect() -> MockServer {
        Self::start(Mode::ConfigDisconnect).await
    }

    async fn start(mode: Mode) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        let echo_count = Arc::new(AtomicUsize::new(0));
        let echo_ids = Arc::new(Mutex::new(Vec::new()));
        let (count_clone, ids_clone) = (echo_count.clone(), echo_ids.clone());
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            run_server(stream, mode, count_clone, ids_clone).await
        });
        MockServer {
            port,
            echo_count,
            echo_ids,
            handle,
        }
    }

    /// Waits for the server task to finish, propagating its error.
    pub async fn finish(self) -> Result<()> {
        self.handle.await.expect("mock server task panicked")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encrypted,
    Plain,
    ConfigDisconnect,
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(MineRiderError::Protocol(format!(
            "mock server: {}",
            msg.into()
        )))
    }
}

async fn run_server(
    stream: TcpStream,
    mode: Mode,
    echo_count: Arc<AtomicUsize>,
    echo_ids: Arc<Mutex<Vec<i64>>>,
) -> Result<()> {
    let mut conn = Connection::from_tcp_stream(stream)?;

    // Handshake (C2S 0x00).
    let hs = conn.read_packet().await?;
    ensure(hs.id == 0x00, "expected handshake packet 0x00")?;
    {
        let mut r = PacketReader::new(&hs.payload);
        let protocol = r.get_varint()?;
        let _address = r.read_string()?;
        let _port = r.get_u16()?;
        let next_state = r.get_varint()?;
        ensure(
            protocol == EXPECTED_PROTOCOL,
            format!("protocol {protocol}, expected {EXPECTED_PROTOCOL}"),
        )?;
        ensure(
            next_state == 2,
            format!("next state {next_state}, expected 2 (login)"),
        )?;
    }

    // Login Start (C2S 0x00).
    let ls = conn.read_packet().await?;
    ensure(ls.id == 0x00, "expected login start 0x00")?;
    {
        let mut r = PacketReader::new(&ls.payload);
        let _username = r.read_string()?;
        let _uuid = r.read_uuid()?;
    }

    if mode == Mode::Encrypted {
        encryption_exchange(&mut conn).await?;
    }

    // Set Compression (S2C 0x03). Sent uncompressed; both sides enable
    // compression for the frames that follow.
    let threshold = match mode {
        Mode::Encrypted => 64,
        Mode::Plain | Mode::ConfigDisconnect => 16,
    };
    let mut w = PacketWriter::new();
    w.put_varint(threshold);
    conn.send_packet(0x03, &w.into_inner()).await?;
    conn.set_compression(threshold);

    // Login Success (S2C 0x02). Payload must be ≥ threshold so the
    // compressed codec path is exercised.
    let username = match mode {
        Mode::Encrypted => format!("MockPlayer{}", "x".repeat(60)),
        Mode::Plain | Mode::ConfigDisconnect => "MockPlayer".to_string(),
    };
    let mut w = PacketWriter::new();
    w.put_uuid(0x00112233445566778899AABBCCDDEEFF);
    w.put_string(&username)?;
    w.put_varint(0); // zero properties
    conn.send_packet(0x02, &w.into_inner()).await?;

    // Login Acknowledged (C2S 0x03).
    let ack = conn.read_packet().await?;
    ensure(
        ack.id == 0x03,
        format!("expected login acknowledged 0x03, got 0x{:02x}", ack.id),
    )?;
    ensure(ack.payload.is_empty(), "login acknowledged must be empty")?;

    // Configuration: Finish Configuration (S2C 0x03), expect C2S 0x03.
    if mode == Mode::ConfigDisconnect {
        // Configuration Disconnect (S2C 0x02) instead: the reason is a
        // network NBT text component (anonymousNbt). Minimal compound
        // `{text:"kicked"}`: TAG_Compound, TAG_String "text" = "kicked",
        // TAG_End.
        let reason_nbt: &[u8] = &[
            0x0A, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x06, b'k', b'i', b'c', b'k',
            b'e', b'd', 0x00,
        ];
        conn.send_packet(0x02, reason_nbt).await?;
        conn.close().await?;
        return Ok(());
    }
    conn.send_packet(0x03, &[]).await?;
    let fin = conn.read_packet().await?;
    ensure(
        fin.id == 0x03,
        format!("expected finish configuration 0x03, got 0x{:02x}", fin.id),
    )?;

    // Play state: keep-alives (S2C 0x27, i64 payload), expect C2S 0x1a echoes.
    let keepalive_ids: &[i64] = match mode {
        Mode::Encrypted => &[42, 43, 44],
        Mode::Plain => &[42],
        Mode::ConfigDisconnect => unreachable!("config-disconnect mock returned earlier"),
    };
    for &id in keepalive_ids {
        let mut w = PacketWriter::new();
        w.put_i64(id);
        conn.send_packet(0x27, &w.into_inner()).await?;

        let echo = conn.read_packet().await?;
        ensure(
            echo.id == 0x1a,
            format!("expected keep-alive echo 0x1a, got 0x{:02x}", echo.id),
        )?;
        let mut r = PacketReader::new(&echo.payload);
        let echoed = r.get_i64()?;
        ensure(echoed == id, format!("echoed id {echoed}, expected {id}"))?;
        echo_count.fetch_add(1, Ordering::SeqCst);
        echo_ids.lock().await.push(echoed);
    }

    conn.close().await?;
    Ok(())
}

async fn encryption_exchange(conn: &mut Connection) -> Result<()> {
    let (public, private) = rsa::generate_keypair(1024)?;
    let der = public
        .to_public_key_der()
        .map_err(|e| MineRiderError::Crypto(format!("mock server: DER encode failed: {e}")))?;
    let verify_token = [0x5A, 0x11, 0x22, 0x33];

    // Encryption Request (S2C 0x01), sent unencrypted.
    let mut w = PacketWriter::new();
    w.put_string("")?;
    w.put_byte_array(der.as_bytes());
    w.put_byte_array(&verify_token);
    w.put_bool(false); // should_authenticate
    conn.send_packet(0x01, &w.into_inner()).await?;

    // Encryption Response (C2S 0x01), still unencrypted.
    let resp = conn.read_packet().await?;
    ensure(resp.id == 0x01, "expected encryption response 0x01")?;
    let (encrypted_secret, encrypted_token) = {
        let mut r = PacketReader::new(&resp.payload);
        let secret = r.read_byte_array()?.to_vec();
        let token = r.read_byte_array()?.to_vec();
        (secret, token)
    };

    let shared_secret = private
        .decrypt(Pkcs1v15Encrypt, &encrypted_secret)
        .map_err(|e| MineRiderError::Crypto(format!("mock server: secret decrypt failed: {e}")))?;
    let token = private
        .decrypt(Pkcs1v15Encrypt, &encrypted_token)
        .map_err(|e| MineRiderError::Crypto(format!("mock server: token decrypt failed: {e}")))?;
    ensure(token == verify_token, "verify token mismatch")?;
    let secret: [u8; 16] = shared_secret.try_into().map_err(|_| {
        MineRiderError::Crypto("mock server: shared secret not 16 bytes".to_string())
    })?;

    conn.enable_encryption(&secret);
    Ok(())
}
