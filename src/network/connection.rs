//! Connection manager: owns the socket, codec state (compression threshold,
//! encryption) and clean shutdown.

use std::time::Duration;

use bytes::BytesMut;
use minerider_protocol::codec::FrameCodec;
use minerider_protocol::crypto::aes::StreamCipher;
use minerider_protocol::packet::RawPacket;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;

use super::tcp::TcpTransport;

/// Default timeout for a single socket read.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for establishing the TCP connection.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes read from the socket per syscall.
const READ_CHUNK: usize = 8 * 1024;

/// A Minecraft connection: framed TCP plus optional compression and
/// AES-128-CFB8 encryption.
///
/// Decryption, once enabled, applies to every incoming byte including
/// frame-length VarInts, so bytes are decrypted immediately after being
/// read from the socket and before any frame parsing happens.
pub struct Connection {
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
    read_buf: BytesMut,
    codec: FrameCodec,
    cipher: Option<StreamCipher>,
    read_timeout: Duration,
    state: ConnectionState,
}

impl Connection {
    /// Connects to `host:port` with the default timeouts.
    pub async fn connect(host: &str, port: u16) -> Result<Connection> {
        let transport = TcpTransport::connect(host, port, DEFAULT_CONNECT_TIMEOUT).await?;
        Ok(Connection {
            read: transport.read,
            write: transport.write,
            read_buf: BytesMut::with_capacity(READ_CHUNK * 2),
            codec: FrameCodec::new(),
            cipher: None,
            read_timeout: DEFAULT_READ_TIMEOUT,
            state: ConnectionState::Handshaking,
        })
    }

    /// Wraps an already-connected stream (used to build the server side of
    /// a connection, e.g. by the test mock server).
    pub fn from_tcp_stream(stream: TcpStream) -> Result<Connection> {
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        Ok(Connection {
            read,
            write,
            read_buf: BytesMut::with_capacity(READ_CHUNK * 2),
            codec: FrameCodec::new(),
            cipher: None,
            read_timeout: DEFAULT_READ_TIMEOUT,
            state: ConnectionState::Handshaking,
        })
    }

    /// Frames and sends one packet, encrypting the frame if a cipher is set.
    pub async fn send_packet(&mut self, id: i32, payload: &[u8]) -> Result<()> {
        let packet = RawPacket::new(id, BytesMut::from(payload));
        let mut frame = self.codec.encode(&packet)?;
        if let Some(cipher) = &mut self.cipher {
            cipher.encrypt(&mut frame);
        }
        self.write.write_all(&frame).await?;
        self.write.flush().await?;
        Ok(())
    }

    /// Reads and returns the next packet.
    ///
    /// Blocks (up to `read_timeout` per socket read) until a complete frame
    /// is buffered and decoded. Returns [`MineRiderError::ConnectionClosed`]
    /// when the peer closes the connection before a full frame arrives.
    pub async fn read_packet(&mut self) -> Result<RawPacket> {
        loop {
            if let Some(packet) = self.codec.try_decode(&mut self.read_buf)? {
                return Ok(packet);
            }
            let mut chunk = [0u8; READ_CHUNK];
            let n = tokio::time::timeout(self.read_timeout, self.read.read(&mut chunk))
                .await
                .map_err(|_| MineRiderError::Timeout("reading from socket".to_string()))??;
            if n == 0 {
                return Err(MineRiderError::ConnectionClosed);
            }
            // Decrypt the fresh bytes in place in the stack buffer, then
            // extend the read buffer — no intermediate allocation.
            if let Some(cipher) = &mut self.cipher {
                cipher.decrypt(&mut chunk[..n]);
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Enables compression with the given threshold (see
    /// [`FrameCodec::set_compression_threshold`]).
    pub fn set_compression(&mut self, threshold: i32) {
        self.codec.set_compression_threshold(threshold);
    }

    /// Enables AES-128-CFB8 encryption for all subsequent traffic in both
    /// directions, using the shared secret as key and IV.
    pub fn enable_encryption(&mut self, shared_secret: &[u8; 16]) {
        self.cipher = Some(StreamCipher::new(shared_secret));
    }

    /// Current protocol state of the connection.
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Sets the protocol state of the connection.
    pub fn set_state(&mut self, state: ConnectionState) {
        self.state = state;
    }

    /// Overrides the per-read timeout (mainly useful for tests).
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    /// Shuts down the write side of the socket.
    pub async fn close(&mut self) -> Result<()> {
        self.write.shutdown().await?;
        Ok(())
    }
}
