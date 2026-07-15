//! Conformance scenarios 1-4: run the client against scenario mock servers
//! with a trace recorder, normalize the capture and compare it against
//! committed fixtures (scenarios 1-2) or assert the required vanilla
//! obligation response (scenarios 3-4).
//!
//! Fixtures regenerate with `MINERIDER_WRITE_FIXTURES=1 cargo test --test conformance`.

mod common;

use std::time::Duration;

use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::state::ConnectionState;
use minerider::trace::decode::packet_name;
use minerider::trace::diff::diff;
use minerider::trace::format::TraceEvent;
use minerider::trace::normalize::normalize;
use minerider::trace::recorder::{read_trace, TraceRecorder};
use minerider::trace::Direction;

const FIXTURES: &str = "tests/conformance/fixtures";

fn write_fixtures() -> bool {
    std::env::var_os("MINERIDER_WRITE_FIXTURES").is_some()
}

/// Runs the client against a started mock server, capturing a normalized
/// trace of the whole session.
async fn capture(server: &MockServer, scenario: &str) -> Vec<TraceEvent> {
    let path = std::env::temp_dir().join(format!(
        "minerider-trace-{scenario}-{}.jsonl",
        std::process::id()
    ));
    let recorder = TraceRecorder::create(&path, "SESSION_1", scenario).expect("create recorder");
    let cfg = ClientConfig::new("127.0.0.1", server.port, "TraceBot");
    let mut client = Client::connect_with_trace(&cfg, recorder)
        .await
        .expect("client connect");
    // The mock closes after its script; run() then ends with an error.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.run()).await;
    drop(client);
    let events = read_trace(&path).expect("read trace");
    std::fs::remove_file(&path).ok();
    normalize(&events)
}

fn fixture_path(scenario: &str) -> String {
    format!("{FIXTURES}/{scenario}.jsonl")
}

/// Compares a fresh capture against the committed fixture, or rewrites the
/// fixture when MINERIDER_WRITE_FIXTURES is set.
fn check_fixture(scenario: &str, captured: &[TraceEvent]) {
    let path = fixture_path(scenario);
    if write_fixtures() {
        std::fs::create_dir_all(FIXTURES).expect("create fixtures dir");
        let mut out = String::new();
        for event in captured {
            out.push_str(&serde_json::to_string(event).expect("serialize event"));
            out.push('\n');
        }
        std::fs::write(&path, out).expect("write fixture");
        return;
    }
    let expected = read_trace(&path).unwrap_or_else(|e| panic!("read fixture {path}: {e}"));
    let report = diff(&expected, captured);
    assert!(
        report.is_match(),
        "scenario {scenario} diverged from fixture after {} events: {:?}",
        report.compared,
        report.divergence
    );
}

fn has_event(trace: &[TraceEvent], dir: Direction, state: ConnectionState, id: i32) -> bool {
    let name = packet_name(state, dir, id).expect("known packet");
    let state_name = match state {
        ConnectionState::Login => "login",
        ConnectionState::Configuration => "configuration",
        ConnectionState::Play => "play",
        _ => unreachable!(),
    };
    trace
        .iter()
        .any(|e| e.dir == dir && e.state == state_name && e.id == id && e.name == name)
}

/// Finds a field by key inside an event's decoded fields, descending
/// through the enum-variant wrapper (`{"KeepAlive": {"id": …}}`).
fn find_field<'a>(event: &'a TraceEvent, key: &str) -> Option<&'a serde_json::Value> {
    fn walk<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
        if let serde_json::Value::Object(map) = value {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            for child in map.values() {
                if let Some(found) = walk(child, key) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(event.fields.as_ref()?, key)
}

// ---------------------------------------------------------------------------
// Scenario 1: configuration completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_1_configuration_completion() {
    let server = MockServer::start_plain().await;
    let captured = capture(&server, "configuration_completion").await;
    server.finish().await.expect("mock server flow failed");

    // The scenario's defining moment: finish_configuration in both
    // directions, then play state begins.
    assert!(has_event(
        &captured,
        Direction::Clientbound,
        ConnectionState::Configuration,
        3
    ));
    assert!(has_event(
        &captured,
        Direction::Serverbound,
        ConnectionState::Configuration,
        3
    ));
    check_fixture("configuration_completion", &captured);
}

// ---------------------------------------------------------------------------
// Scenario 2: join and stand still
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_2_join_and_idle() {
    let server = MockServer::start_join_idle().await;
    let captured = capture(&server, "join_idle").await;
    server.finish().await.expect("mock server flow failed");

    // Play login, initial world readiness and vanilla idle movement occurred;
    // three keep-alives were then echoed.
    assert!(has_event(
        &captured,
        Direction::Clientbound,
        ConnectionState::Play,
        44
    ));
    let loaded_pos = captured
        .iter()
        .position(|e| e.dir == Direction::Serverbound && e.state == "play" && e.id == 42)
        .expect("player_loaded in capture");
    let movement_pos = captured
        .iter()
        .position(|e| e.dir == Direction::Serverbound && e.state == "play" && e.id == 28)
        .expect("idle position reminder in capture");
    assert!(
        loaded_pos < movement_pos,
        "movement must follow player_loaded readiness"
    );
    assert_eq!(
        captured
            .iter()
            .filter(|e| e.dir == Direction::Serverbound && e.state == "play" && e.id == 42)
            .count(),
        1,
        "player_loaded is sent once"
    );

    let echoes: Vec<_> = captured
        .iter()
        .filter(|e| e.dir == Direction::Serverbound && e.state == "play" && e.name == "keep_alive")
        .collect();
    assert_eq!(echoes.len(), 3, "expected 3 keep-alive echoes");
    // Normalization correlated each echo with its trigger: the same
    // KEEPALIVE_ID_N symbol appears in both directions.
    for echo in &echoes {
        let id = find_field(echo, "keep_alive_id").expect("echo has keep_alive_id");
        assert!(
            captured.iter().any(|e| {
                e.dir == Direction::Clientbound
                    && e.name == "keep_alive"
                    && find_field(e, "keep_alive_id") == Some(id)
            }),
            "echo id {id} has no matching clientbound keep-alive"
        );
    }
    check_fixture("join_idle", &captured);
}

// ---------------------------------------------------------------------------
// Scenario 3: initial chunk streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_3_initial_chunks() {
    let server = MockServer::start_chunk_streaming().await;
    let captured = capture(&server, "initial_chunks").await;
    // The mock asserts chunk_batch_received arrived with a valid
    // chunks-per-tick value; a failure here means the client did not
    // acknowledge the batch.
    server.finish().await.expect("mock server flow failed");

    // The chunk batch arrived and was decoded.
    assert!(has_event(
        &captured,
        Direction::Clientbound,
        ConnectionState::Play,
        13
    )); // batch start
    assert!(has_event(
        &captured,
        Direction::Clientbound,
        ConnectionState::Play,
        12
    )); // batch finished

    // The acknowledgement was sent immediately after batch finished.
    let finished_pos = captured
        .iter()
        .position(|e| e.dir == Direction::Clientbound && e.state == "play" && e.id == 12)
        .expect("chunk_batch_finished in capture");
    let ack = captured
        .get(finished_pos + 1)
        .expect("event after chunk_batch_finished");
    assert_eq!(ack.dir, Direction::Serverbound);
    assert_eq!(
        ack.id, 9,
        "expected chunk_batch_received right after batch finished"
    );
    assert_eq!(ack.name, "chunk_batch_received");
    check_fixture("initial_chunks", &captured);
}

// ---------------------------------------------------------------------------
// Scenario 4: teleport correction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scenario_4_teleport_correction() {
    let server = MockServer::start_teleport_correction().await;
    let captured = capture(&server, "teleport_correction").await;
    // The mock asserts the confirmation arrived with the right id; a
    // failure here means the client did not confirm the teleport.
    server.finish().await.expect("mock server flow failed");

    // The position packet arrived and was decoded with teleport id 1.
    let position = captured
        .iter()
        .find(|e| e.dir == Direction::Clientbound && e.state == "play" && e.id == 66)
        .expect("synchronize_player_position in capture");
    assert_eq!(
        find_field(position, "teleport_id"),
        Some(&serde_json::Value::String("TELEPORT_ID_1".to_string()))
    );

    // The confirmation echoed the same (normalized) teleport id.
    let confirm = captured
        .iter()
        .find(|e| e.dir == Direction::Serverbound && e.state == "play" && e.id == 0)
        .expect("teleport_confirm in capture");
    assert_eq!(
        find_field(confirm, "teleport_id"),
        find_field(position, "teleport_id"),
        "teleport_confirm must echo the server's teleport id"
    );
    check_fixture("teleport_correction", &captured);
}
