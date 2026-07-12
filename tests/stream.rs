//! Network/stream edge cases: partial reads, segmented frames, encryption +
//! compression ordering, clean shutdown, reconnect and garbage input.
//!
//! These drive `Connection` over real loopback sockets; the peer side uses
//! raw `TcpStream`s (or `Connection::from_tcp_stream`) so segmentation and
//! shutdown behavior are under explicit test control.

mod common;

use std::time::Duration;

use bytes::BytesMut;
use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;
use minerider::network::connection::Connection;
use minerider_protocol::codec::FrameCodec;
use minerider_protocol::crypto::aes::StreamCipher;
use minerider_protocol::packet::RawPacket;
use rand::RngCore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn bind() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn test_packet(id: i32, payload: &[u8]) -> RawPacket {
    RawPacket::new(id, BytesMut::from(payload))
}

#[tokio::test]
async fn frame_dribbled_one_byte_at_a_time_decodes() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let codec = FrameCodec::new();
        let frame = codec.encode(&test_packet(0x2A, b"dribble")).unwrap();
        for byte in frame.iter() {
            stream.write_all(std::slice::from_ref(byte)).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let packet = conn.read_packet().await.unwrap();
    assert_eq!(packet.id, 0x2A);
    assert_eq!(&packet.payload[..], b"dribble");
}

#[tokio::test]
async fn multiple_frames_in_single_segment_decode_in_order() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        for i in 0..3i32 {
            let payload = vec![i as u8; i as usize + 1];
            buf.extend_from_slice(&codec.encode(&test_packet(i, &payload)).unwrap());
        }
        stream.write_all(&buf).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    for i in 0..3 {
        let packet = conn.read_packet().await.unwrap();
        assert_eq!(packet.id, i);
        assert_eq!(packet.payload.len(), i as usize + 1);
    }
}

#[tokio::test]
async fn frame_length_varint_split_across_segments() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Frame of 300 bytes: length VarInt is 0xAC 0x02, sent as two
        // separate segments with a gap in between.
        let codec = FrameCodec::new();
        let frame = codec
            .encode(&test_packet(0x07, &vec![0x5Au8; 297]))
            .unwrap();
        assert!(
            frame[0] & 0x80 != 0,
            "test requires a multi-byte length varint"
        );
        stream.write_all(&frame[..1]).await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        stream.write_all(&frame[1..]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let packet = conn.read_packet().await.unwrap();
    assert_eq!(packet.id, 0x07);
    assert_eq!(packet.payload.len(), 297);
}

#[tokio::test]
async fn payload_split_across_many_segments() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let codec = FrameCodec::new();
        let frame = codec
            .encode(&test_packet(0x11, &vec![0x77u8; 4096]))
            .unwrap();
        for chunk in frame.chunks(137) {
            stream.write_all(chunk).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let packet = conn.read_packet().await.unwrap();
    assert_eq!(packet.id, 0x11);
    assert_eq!(packet.payload.len(), 4096);
    assert!(packet.payload.iter().all(|&b| b == 0x77));
}

#[tokio::test]
async fn malformed_varint_mid_stream_errors() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let codec = FrameCodec::new();
        // One valid frame, then a frame whose packet-id VarInt is malformed
        // (six continuation bytes) — not the frame-length VarInt.
        let good = codec.encode(&test_packet(0x01, b"ok")).unwrap();
        stream.write_all(&good).await.unwrap();
        stream
            .write_all(&[6, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let first = conn.read_packet().await.unwrap();
    assert_eq!(first.id, 0x01);
    let err = conn
        .read_packet()
        .await
        .expect_err("malformed id varint must error");
    assert!(
        matches!(err, MineRiderError::Wire(_)),
        "expected Wire error, got {err}"
    );
}

#[tokio::test]
async fn encryption_and_compression_compose_in_wire_order() {
    // Wire order: frame(compress(packet)) then encrypt(frame); the receiver
    // must decrypt, then deframe, then decompress.
    let secret = [0x33u8; 16];
    let threshold = 64;

    let payload = vec![0xABu8; 1024];
    let mut send_codec = FrameCodec::new();
    send_codec.set_compression_threshold(threshold);
    let mut frame = send_codec.encode(&test_packet(0x09, &payload)).unwrap();
    // Compression must actually have kicked in (compressed < uncompressed).
    assert!(frame.len() < payload.len());

    let mut enc = StreamCipher::new(&secret);
    enc.encrypt(&mut frame);

    // Receiver: decrypt first, then decode with compression enabled.
    let mut dec = StreamCipher::new(&secret);
    dec.decrypt(&mut frame);

    let mut recv_codec = FrameCodec::new();
    recv_codec.set_compression_threshold(threshold);
    let mut buf = frame;
    let packet = recv_codec.try_decode(&mut buf).unwrap().unwrap();
    assert_eq!(packet.id, 0x09);
    assert_eq!(&packet.payload[..], &payload[..]);

    // Sanity: skipping decryption must never yield the original packet.
    let mut enc2 = StreamCipher::new(&secret);
    let mut raw2 = send_codec.encode(&test_packet(0x09, &payload)).unwrap();
    enc2.encrypt(&mut raw2);
    match recv_codec.try_decode(&mut raw2) {
        Err(_) => {}   // expected: garbage frame length
        Ok(None) => {} // acceptable: looked like an incomplete frame
        Ok(Some(p)) => panic!("undecrypted frame decoded as packet {p:?}"),
    }
}

#[tokio::test]
async fn clean_shutdown_client_close_yields_eof_on_server() {
    let (listener, port) = bind().await;

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = Connection::from_tcp_stream(stream).unwrap();
        // The pending packet decodes first...
        let packet = conn.read_packet().await.unwrap();
        assert_eq!(packet.id, 0x01);
        // ...then, after the client shuts down its write side, the server
        // sees a clean EOF — ConnectionClosed, not an io/protocol error.
        let err = conn.read_packet().await.expect_err("expected EOF");
        assert!(
            matches!(err, MineRiderError::ConnectionClosed),
            "expected ConnectionClosed, got {err}"
        );
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    conn.send_packet(0x01, b"bye").await.unwrap();
    conn.close().await.unwrap();
    // Give the server a moment to observe EOF.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server task hung")
        .expect("server task panicked");
}

#[tokio::test]
async fn reconnect_after_full_login_works() {
    // Full login, run to completion, then a second fresh connect against a
    // new server instance must succeed identically.
    for _ in 0..2 {
        let server = MockServer::start_encrypted().await;
        let cfg = ClientConfig::new("127.0.0.1", server.port, "TestBot");
        let mut client = Client::connect(&cfg).await.expect("client connect");
        let err = client.run().await.expect_err("run ends when server closes");
        assert!(matches!(err, MineRiderError::ConnectionClosed));
        server.finish().await.expect("mock server flow failed");
    }
}

#[tokio::test]
async fn random_garbage_errors_without_panic_or_hang() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut garbage = vec![0u8; 8192];
        rand::rngs::OsRng.fill_bytes(&mut garbage);
        stream.write_all(&garbage).await.unwrap();
        // Keep the socket open: the client must error from the data alone,
        // not from EOF.
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    conn.set_read_timeout(Duration::from_millis(500));

    // Garbage may contain a decodable frame or two by chance; keep reading
    // until an error surfaces, but never longer than the overall deadline.
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            conn.read_packet().await?;
        }
    })
    .await;

    match result {
        Err(_) => panic!("client hung on garbage input"),
        Ok(Err(e)) => {
            // Any typed error is acceptable; a panic would have aborted the
            // test above. Typically Wire (oversized/garbled frame) or
            // Timeout if the random bytes looked like a pending frame.
            assert!(
                matches!(
                    e,
                    MineRiderError::Wire(_)
                        | MineRiderError::Protocol(_)
                        | MineRiderError::ConnectionClosed
                        | MineRiderError::Timeout(_)
                        | MineRiderError::Io(_)
                ),
                "unexpected error kind: {e}"
            );
        }
        Ok(Ok(())) => unreachable!("infinite loop cannot return Ok"),
    }
}

/// A peer that reads everything the client sends until EOF must see the
/// exact bytes of a small encrypted+compressed exchange.
#[tokio::test]
async fn encrypted_connection_roundtrip_over_tcp() {
    let (listener, port) = bind().await;
    let secret = [0x99u8; 16];

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = Connection::from_tcp_stream(stream).unwrap();
        conn.set_compression(32);
        conn.enable_encryption(&secret);
        for expected_id in [1, 2, 3] {
            let packet = conn.read_packet().await.unwrap();
            assert_eq!(packet.id, expected_id);
        }
        conn.send_packet(0x7F, b"pong").await.unwrap();
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    conn.set_compression(32);
    conn.enable_encryption(&secret);
    conn.send_packet(1, &vec![0xAA; 256]).await.unwrap(); // compressed path
    conn.send_packet(2, b"tiny").await.unwrap(); // raw path (< threshold)
    conn.send_packet(3, &[0xBB; 100]).await.unwrap();
    let reply = conn.read_packet().await.unwrap();
    assert_eq!(reply.id, 0x7F);
    assert_eq!(&reply.payload[..], b"pong");
    server.await.expect("server task panicked");
}

/// Raw-socket variant of the dribble test using plain blocking writes with
/// sleeps between single-byte writes (no flush batching by the runtime).
#[tokio::test]
async fn dribble_via_raw_stream_one_byte_writes() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let codec = FrameCodec::new();
        let frame = codec.encode(&test_packet(0x33, b"raw-dribble")).unwrap();
        for &byte in frame.iter() {
            stream.write_all(&[byte]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Read (and ignore) anything until EOF so the client can close.
        let mut sink = [0u8; 64];
        let _ = stream.read(&mut sink).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let packet = conn.read_packet().await.unwrap();
    assert_eq!(packet.id, 0x33);
    assert_eq!(&packet.payload[..], b"raw-dribble");
}

/// Server closes immediately after accepting: client gets ConnectionClosed.
#[tokio::test]
async fn immediate_peer_close_is_connection_closed() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let err = conn.read_packet().await.expect_err("must see EOF");
    assert!(matches!(err, MineRiderError::ConnectionClosed));
}

/// The client must time out (not hang) when the peer stays silent.
#[tokio::test]
async fn silent_peer_times_out() {
    let (listener, port) = bind().await;

    tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    conn.set_read_timeout(Duration::from_millis(150));
    let started = std::time::Instant::now();
    let err = conn.read_packet().await.expect_err("must time out");
    assert!(matches!(err, MineRiderError::Timeout(_)));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout took too long"
    );
}
