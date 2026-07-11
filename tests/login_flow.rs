//! Full login flow against the encrypted mock server: handshake →
//! encryption → compression → configuration → play keep-alives.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::state::ConnectionState;

#[tokio::test]
async fn client_reaches_play_and_echoes_keepalives() {
    let server = MockServer::start_encrypted().await;
    let cfg = ClientConfig::new("127.0.0.1", server.port, "TestBot");

    let mut client = Client::connect(&cfg)
        .await
        .expect("client connect should succeed");
    assert_eq!(client.state(), ConnectionState::Play);

    // Race the play loop against a timeout: the mock closes the connection
    // after three keep-alives, which ends the loop with ConnectionClosed.
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

    assert!(server.echo_count.load(Ordering::SeqCst) >= 3, "expected >= 3 keep-alive echoes");
    assert_eq!(client.state(), ConnectionState::Play);
    server.finish().await.expect("mock server flow failed");
}
