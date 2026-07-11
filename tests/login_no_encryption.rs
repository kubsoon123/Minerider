//! Login flow without an encryption request: the mock goes straight to
//! SetCompression + LoginSuccess, which the client must accept.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::state::ConnectionState;

#[tokio::test]
async fn client_handles_server_without_encryption() {
    let server = MockServer::start_plain().await;
    let cfg = ClientConfig::new("127.0.0.1", server.port, "TestBot");

    let mut client = Client::connect(&cfg)
        .await
        .expect("client connect should succeed without encryption");
    assert_eq!(client.state(), ConnectionState::Play);
    assert_eq!(client.username, "MockPlayer");

    tokio::select! {
        result = client.run() => {
            let err = result.expect_err("run() must end when the server closes");
            assert!(
                matches!(err, minerider::core::error::MineRiderError::ConnectionClosed),
                "expected ConnectionClosed, got {err}"
            );
        }
        () = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("client.run() did not finish; mock observed {} echoes", server.echo_count.load(Ordering::SeqCst));
        }
    }

    // The plain mock's keep-alive is 9 bytes on the wire (< threshold 16),
    // exercising the uncompressed path while LoginSuccess (29 bytes) took
    // the compressed path.
    assert!(server.echo_count.load(Ordering::SeqCst) >= 1, "expected >= 1 keep-alive echo");
    assert_eq!(client.state(), ConnectionState::Play);
    server.finish().await.expect("mock server flow failed");
}
