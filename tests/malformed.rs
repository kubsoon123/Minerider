//! Malformed-traffic handling at the raw connection level.

use std::time::Duration;

use minerider::core::error::MineRiderError;
use minerider::network::connection::Connection;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn oversized_frame_length_errors_with_protocol() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Frame length VarInt claiming 3 MiB (0x300000 → 0x80 0x80 0xC0
        // 0x01), followed by a little garbage. Exceeds the 2 MiB codec limit.
        stream.write_all(&[0x80, 0x80, 0xC0, 0x01, 0x00, 0x00, 0x00]).await.unwrap();
        // Keep the socket open briefly so the client reads before close.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let err = conn.read_packet().await.expect_err("must reject oversized frame");
    assert!(
        matches!(err, MineRiderError::Protocol(_)),
        "expected Protocol error, got {err}"
    );
}

#[tokio::test]
async fn peer_closing_mid_frame_errors_with_connection_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Frame length 100, then only 10 body bytes, then close.
        stream.write_all(&[100, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).await.unwrap();
        stream.shutdown().await.unwrap();
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let err = conn.read_packet().await.expect_err("must detect closed connection");
    assert!(
        matches!(err, MineRiderError::ConnectionClosed),
        "expected ConnectionClosed, got {err}"
    );
}

#[tokio::test]
async fn malformed_frame_length_varint_errors_with_protocol() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Six continuation bytes: not a valid VarInt.
        stream.write_all(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut conn = Connection::connect("127.0.0.1", port).await.unwrap();
    let err = conn.read_packet().await.expect_err("must reject malformed varint");
    assert!(
        matches!(err, MineRiderError::Protocol(_)),
        "expected Protocol error, got {err}"
    );
}
