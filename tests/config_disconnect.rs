//! Configuration-state disconnect: the mock kicks the client with a
//! Disconnect packet whose reason is a network NBT text component; the
//! client must surface it as `Disconnected` without a parse error.

mod common;

use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;

#[tokio::test]
async fn configuration_disconnect_with_nbt_reason_surfaces() {
    let server = MockServer::start_config_disconnect().await;
    let cfg = ClientConfig::new("127.0.0.1", server.port, "TestBot");

    let err = match Client::connect(&cfg).await {
        Err(err) => err,
        Ok(_) => panic!("connect must fail when the server disconnects during configuration"),
    };
    match err {
        MineRiderError::Disconnected(reason) => {
            // The raw NBT payload is kept as lossy UTF-8 until Phase 2; the
            // component text must survive in the message.
            assert!(
                reason.contains("kicked"),
                "expected reason to contain the component text, got {reason:?}"
            );
        }
        other => panic!("expected Disconnected, got {other}"),
    }

    server.finish().await.expect("mock server flow failed");
}
