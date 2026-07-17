//! Connection manager: owns the socket, codec state (compression threshold,
//! encryption) and clean shutdown.

use std::time::Duration;

use bytes::BytesMut;
use minerider_protocol::codec::FrameCodec;
use minerider_protocol::crypto::aes::StreamCipher;
use minerider_protocol::packet::RawPacket;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::network::socks5::{self, Socks5ProxyConfig};
use crate::trace::format::Direction;
use crate::trace::recorder::TraceRecorder;

use super::tcp::TcpTransport;

/// Default timeout for a single socket read.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for establishing the TCP connection.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for a complete packet send (`write_all` + `flush`
/// together, against one shared deadline — see [`write_frame_with_timeout`]).
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes read from the socket per syscall.
const READ_CHUNK: usize = 8 * 1024;

/// The timeouts a [`Connection`] enforces. Grouped so callers configure all
/// three together (e.g. from [`crate::core::client::ClientConfig`]) instead
/// of three separate setter calls.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionTimeouts {
    /// Bound on establishing the TCP connection itself.
    pub connect: Duration,
    /// Bound on a single socket read while assembling one frame.
    pub read: Duration,
    /// Bound on a complete packet send (`write_all` + `flush`).
    pub write: Duration,
}

impl Default for ConnectionTimeouts {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            read: DEFAULT_READ_TIMEOUT,
            write: DEFAULT_WRITE_TIMEOUT,
        }
    }
}

/// Runs `write_all` then `flush` against one shared deadline computed once,
/// so the whole send operation — not each step separately — is bounded by
/// `timeout`. Generic over `AsyncWrite` so it can be exercised directly with
/// an in-memory fake in tests, without any real socket or timing flakiness;
/// [`Connection::send_packet`] is the realistic call site using a real
/// `OwnedWriteHalf`.
///
/// On timeout, the returned error names which step ("write_all" or "flush")
/// was in flight, since a caller deciding whether a partial send is safe to
/// retry needs to know that, not just that "something timed out".
async fn write_frame_with_timeout<W: AsyncWrite + Unpin>(
    write: &mut W,
    frame: &[u8],
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, write.write_all(frame))
        .await
        .map_err(|_| {
            MineRiderError::Timeout("writing packet to socket (write_all)".to_string())
        })??;
    tokio::time::timeout_at(deadline, write.flush())
        .await
        .map_err(|_| MineRiderError::Timeout("writing packet to socket (flush)".to_string()))??;
    Ok(())
}

/// Whether a failed send should poison the connection (see
/// [`Connection::write_failed`]). Only a timeout is ambiguous about how many
/// bytes actually reached the peer; a plain I/O error (e.g. a reset
/// connection) means the socket is already unusable for an unrelated reason
/// that further calls will independently and immediately hit anyway, so
/// there's nothing extra to guard against there.
fn error_poisons_connection(e: &MineRiderError) -> bool {
    matches!(e, MineRiderError::Timeout(_))
}

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
    write_timeout: Duration,
    /// Set once a write times out. A timed-out `write_all`/`flush` may have
    /// already pushed a prefix of the frame to the OS socket buffer before
    /// giving up, and for an encrypted connection the CFB8 keystream has
    /// already advanced past that frame regardless of how many bytes made it
    /// out — there is no way to prove the peer's frame boundary is still
    /// intact. Rather than risk silently desyncing the wire, every further
    /// `send_packet`/`read_packet` call fails fast once this is set.
    write_failed: bool,
    state: ConnectionState,
    trace: Option<TraceRecorder>,
}

impl Connection {
    /// Connects to `host:port` with the default timeouts.
    pub async fn connect(host: &str, port: u16) -> Result<Connection> {
        Self::connect_with_timeouts(host, port, ConnectionTimeouts::default()).await
    }

    /// Connects to `host:port` with caller-chosen timeouts (see
    /// [`ConnectionTimeouts`]).
    pub async fn connect_with_timeouts(
        host: &str,
        port: u16,
        timeouts: ConnectionTimeouts,
    ) -> Result<Connection> {
        let transport = TcpTransport::connect(host, port, timeouts.connect).await?;
        Ok(Connection {
            read: transport.read,
            write: transport.write,
            read_buf: BytesMut::with_capacity(READ_CHUNK * 2),
            codec: FrameCodec::new(),
            cipher: None,
            read_timeout: timeouts.read,
            write_timeout: timeouts.write,
            write_failed: false,
            state: ConnectionState::Handshaking,
            trace: None,
        })
    }

    /// Connects to `host:port` through a SOCKS5 proxy instead of directly:
    /// TCP-connects to `proxy`, negotiates SOCKS5, and issues `CONNECT
    /// host:port` — the proxy, not this process, resolves `host` when it
    /// isn't already a literal IP (see [`socks5::connect`]). The returned
    /// `Connection` is otherwise identical to one from
    /// [`Self::connect_with_timeouts`]: `host`/`port` here only pick the
    /// SOCKS5 tunnel's destination, so callers (see
    /// [`crate::core::client::Client::connect`]) still send the *original*
    /// Minecraft hostname/port in the handshake, never the proxy's.
    pub async fn connect_via_proxy(
        host: &str,
        port: u16,
        proxy: &Socks5ProxyConfig,
        timeouts: ConnectionTimeouts,
    ) -> Result<Connection> {
        let stream = socks5::connect(proxy, host, port, timeouts.connect).await?;
        Self::from_tcp_stream(stream)
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
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            write_failed: false,
            state: ConnectionState::Handshaking,
            trace: None,
        })
    }

    /// Attaches a trace recorder; every subsequent packet in both
    /// directions is written to the trace.
    pub fn set_trace(&mut self, recorder: TraceRecorder) {
        self.trace = Some(recorder);
    }

    /// Mutable access to the trace recorder (scenario/step markers).
    pub fn trace_mut(&mut self) -> Option<&mut TraceRecorder> {
        self.trace.as_mut()
    }

    /// Frames and sends one packet, encrypting the frame if a cipher is set.
    ///
    /// The whole send (`write_all` + `flush`) is bounded by the connection's
    /// write timeout; see [`write_frame_with_timeout`]. If it times out, this
    /// connection is marked failed (see [`Connection::write_failed`]) and
    /// every subsequent call — on either direction — errors immediately
    /// rather than risk sending or reading against a desynced wire.
    pub async fn send_packet(&mut self, id: i32, payload: &[u8]) -> Result<()> {
        self.check_not_failed()?;
        let packet = RawPacket::new(id, BytesMut::from(payload));
        let mut frame = self.codec.encode(&packet)?;
        if let Some(cipher) = &mut self.cipher {
            cipher.encrypt(&mut frame);
        }
        if let Err(e) = write_frame_with_timeout(&mut self.write, &frame, self.write_timeout).await
        {
            if error_poisons_connection(&e) {
                self.write_failed = true;
            }
            return Err(e);
        }
        if let Some(trace) = &mut self.trace {
            trace.record(
                Direction::Serverbound,
                self.state,
                id,
                payload,
                self.cipher.is_some(),
                self.codec.compression_threshold().is_some(),
            );
        }
        Ok(())
    }

    /// Returns an error without touching the socket if a previous write
    /// timed out and left this connection in an unknown/unsafe state.
    fn check_not_failed(&self) -> Result<()> {
        if self.write_failed {
            Err(MineRiderError::Timeout(
                "connection unusable after a previous write timeout".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    /// Reads and returns the next packet.
    ///
    /// Blocks (up to `read_timeout` per socket read) until a complete frame
    /// is buffered and decoded. Returns [`MineRiderError::ConnectionClosed`]
    /// when the peer closes the connection before a full frame arrives.
    pub async fn read_packet(&mut self) -> Result<RawPacket> {
        self.check_not_failed()?;
        loop {
            if let Some(packet) = self.codec.try_decode(&mut self.read_buf)? {
                if let Some(trace) = &mut self.trace {
                    trace.record(
                        Direction::Clientbound,
                        self.state,
                        packet.id,
                        &packet.payload,
                        self.cipher.is_some(),
                        self.codec.compression_threshold().is_some(),
                    );
                }
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

    /// Overrides the packet-send timeout (mainly useful for tests).
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        self.write_timeout = timeout;
    }

    /// Whether a previous write timed out, leaving this connection unusable.
    pub fn is_write_failed(&self) -> bool {
        self.write_failed
    }

    /// Shuts down the write side of the socket.
    pub async fn close(&mut self) -> Result<()> {
        self.write.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Wraps any `AsyncWrite` but makes `poll_flush` pend forever, so a test
    /// can deterministically exercise the "flush" stage of
    /// [`write_frame_with_timeout`] specifically (a real socket or
    /// `tokio::io::duplex` flushes essentially instantly once `write_all`
    /// completes, so there is no other way to distinguish the two stages'
    /// error messages without real, flaky OS-level backpressure).
    struct FlushNeverCompletes<W>(W);

    impl<W: AsyncWrite + Unpin> AsyncWrite for FlushNeverCompletes<W> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
        }
    }

    /// Short enough that these tests stay fast, long enough that scheduling
    /// jitter can never make a genuinely-completed write look like a
    /// timeout. The blocking itself (duplex backpressure, `Poll::Pending`
    /// flush) is deterministic by construction, not timing-dependent —
    /// only "how long we wait before declaring a timeout" is a real
    /// duration, and it's a two-order-of-magnitude margin.
    const TEST_TIMEOUT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn write_within_timeout_succeeds() {
        // A duplex buffer large enough that write_all completes without
        // needing a reader on the other end; flush on a duplex is a no-op.
        let (mut a, _b) = tokio::io::duplex(64);
        write_frame_with_timeout(&mut a, b"hello", Duration::from_secs(5))
            .await
            .expect("small write within a generous timeout must succeed");
    }

    #[tokio::test]
    async fn write_all_hang_times_out_and_names_the_stage() {
        // A 1-byte duplex buffer with nobody ever reading the other half:
        // writing more than 1 byte blocks forever, deterministically (an
        // in-memory channel with no reader, not a race against a real OS
        // socket buffer's size).
        let (mut a, _b) = tokio::io::duplex(1);
        let err = write_frame_with_timeout(&mut a, b"too many bytes to fit here", TEST_TIMEOUT)
            .await
            .expect_err("must time out, not hang forever");
        match err {
            MineRiderError::Timeout(msg) => assert!(
                msg.contains("write_all"),
                "error must name the write_all stage, got: {msg}"
            ),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn flush_hang_times_out_and_names_the_stage() {
        let (a, _b) = tokio::io::duplex(64);
        let mut wrapped = FlushNeverCompletes(a);
        let err = write_frame_with_timeout(&mut wrapped, b"fits fine", TEST_TIMEOUT)
            .await
            .expect_err("a stuck flush must time out too");
        match err {
            MineRiderError::Timeout(msg) => assert!(
                msg.contains("flush"),
                "error must name the flush stage, got: {msg}"
            ),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn only_timeouts_poison_the_connection() {
        // A timeout leaves the number of bytes actually written to the peer
        // unknown, so it must poison the connection.
        assert!(error_poisons_connection(&MineRiderError::Timeout(
            "writing packet to socket (write_all)".to_string()
        )));
        // A plain I/O error (e.g. connection reset) means the socket is
        // already independently unusable; every other error variant here
        // likewise isn't specific to "we don't know how much of the frame
        // made it out", so none of them need the extra poisoning guard.
        assert!(!error_poisons_connection(&MineRiderError::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")
        )));
        assert!(!error_poisons_connection(&MineRiderError::ConnectionClosed));
        assert!(!error_poisons_connection(&MineRiderError::Disconnected(
            "bye".to_string()
        )));
    }

    // A realistic-connection-path test that actually triggers the write
    // timeout was attempted (a real TCP loopback pair, with the server side
    // never reading) and dropped: this platform's real loopback throughput
    // and OS-level auto-tuned buffers reliably absorbed even a 64 MiB
    // payload in well under 50ms, so no payload size/timeout combination
    // tried here induced backpressure deterministically — exactly the
    // flakiness this project's own test-writing guidance warns against.
    // Every other integration test in this workspace (`login_flow`,
    // `stream`, `conformance`, ...) already exercises `send_packet` over a
    // real socket successfully, which is the realistic-path coverage this
    // module relies on; the timeout *mechanism* itself is proven above
    // against deterministic in-memory backpressure instead.
}
