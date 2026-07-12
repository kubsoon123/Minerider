//! Tests for trace normalization and the semantic diff engine.

use minerider::trace::diff::{diff, DivergenceKind};
use minerider::trace::format::TraceEvent;
use minerider::trace::normalize::normalize;
use minerider::trace::Direction;
use serde_json::json;

fn event(dir: Direction, state: &str, id: i32, name: &str, ts_rel_ms: f64) -> TraceEvent {
    TraceEvent {
        ts_mono_ms: 1_700_000.0,
        ts_rel_ms,
        tick: 0,
        dir,
        state: state.to_string(),
        id,
        name: name.to_string(),
        fields: None,
        payload_hex: "deadbeef".to_string(),
        encrypted: false,
        compressed: false,
        size: 4,
        session: "sess-abc".to_string(),
        scenario: "test".to_string(),
        step: 0,
    }
}

fn with_fields(mut event: TraceEvent, fields: serde_json::Value) -> TraceEvent {
    event.fields = Some(fields);
    event
}

#[test]
fn normalize_redacts_payloads_and_absolute_time() {
    let events = vec![event(
        Direction::Clientbound,
        "play",
        39,
        "keep_alive",
        10.0,
    )];
    let normalized = normalize(&events);
    assert_eq!(normalized[0].ts_mono_ms, 0.0);
    assert!(normalized[0].payload_hex.is_empty());
    assert_eq!(normalized[0].session, "SESSION_1");
    assert_eq!(normalized[0].ts_rel_ms, 10.0);
}

#[test]
fn normalize_maps_same_value_to_same_symbol() {
    let uuid = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
    let events = vec![
        with_fields(
            event(Direction::Clientbound, "play", 44, "login", 1.0),
            json!({"uuid": uuid}),
        ),
        with_fields(
            event(Direction::Clientbound, "play", 1, "spawn_entity", 2.0),
            json!({"uuid": uuid, "entity_id": 42}),
        ),
    ];
    let normalized = normalize(&events);
    let first = &normalized[0].fields.as_ref().unwrap()["uuid"];
    let second = &normalized[1].fields.as_ref().unwrap()["uuid"];
    assert_eq!(first, second);
    assert_eq!(first, "UUID_1");
    assert_eq!(
        normalized[1].fields.as_ref().unwrap()["entity_id"],
        "ENTITY_ID_1"
    );
}

#[test]
fn normalize_correlates_teleport_ids_across_directions() {
    let events = vec![
        with_fields(
            event(
                Direction::Clientbound,
                "play",
                66,
                "synchronize_player_position",
                5.0,
            ),
            json!({"teleport_id": 7}),
        ),
        with_fields(
            event(
                Direction::Serverbound,
                "play",
                0,
                "accept_teleportation",
                6.0,
            ),
            json!({"teleport_id": 7}),
        ),
    ];
    let normalized = normalize(&events);
    assert_eq!(
        normalized[0].fields.as_ref().unwrap()["teleport_id"],
        "TELEPORT_ID_1"
    );
    assert_eq!(
        normalized[1].fields.as_ref().unwrap()["teleport_id"],
        "TELEPORT_ID_1"
    );
}

#[test]
fn normalize_correlates_keepalive_echo() {
    let events = vec![
        with_fields(
            event(Direction::Clientbound, "play", 39, "keep_alive", 5.0),
            json!({"id": 123456789}),
        ),
        with_fields(
            event(Direction::Serverbound, "play", 26, "keep_alive", 6.0),
            json!({"id": 123456789}),
        ),
    ];
    let normalized = normalize(&events);
    let a = &normalized[0].fields.as_ref().unwrap()["id"];
    let b = &normalized[1].fields.as_ref().unwrap()["id"];
    assert_eq!(a, b);
    assert_eq!(a, "KEEPALIVE_ID_1");
}

#[test]
fn diff_identical_traces_match() {
    let trace = vec![
        event(
            Direction::Clientbound,
            "configuration",
            3,
            "finish_configuration",
            10.0,
        ),
        event(
            Direction::Serverbound,
            "configuration",
            3,
            "finish_configuration",
            11.0,
        ),
    ];
    let report = diff(&trace, &trace);
    assert!(report.is_match());
    assert_eq!(report.compared, 2);
    assert_eq!(report.fallout, 0);
}

#[test]
fn diff_detects_missing_packet() {
    let expected = vec![
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        event(Direction::Serverbound, "play", 26, "keep_alive", 11.0),
        event(Direction::Clientbound, "play", 39, "keep_alive", 20.0),
    ];
    let actual = vec![event(
        Direction::Clientbound,
        "play",
        39,
        "keep_alive",
        10.0,
    )];
    let report = diff(&expected, &actual);
    assert_eq!(
        report.divergence.as_ref().map(|d| d.kind),
        Some(DivergenceKind::MissingPacket)
    );
    assert_eq!(report.compared, 1);
    assert!(report.fallout >= 1);
}

#[test]
fn diff_detects_extra_packet() {
    let expected = vec![event(
        Direction::Clientbound,
        "play",
        39,
        "keep_alive",
        10.0,
    )];
    let actual = vec![
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        event(Direction::Serverbound, "play", 26, "keep_alive", 11.0),
    ];
    let report = diff(&expected, &actual);
    assert_eq!(
        report.divergence.as_ref().map(|d| d.kind),
        Some(DivergenceKind::ExtraPacket)
    );
}

#[test]
fn diff_detects_wrong_field_value() {
    let expected = vec![with_fields(
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        json!({"id": "KEEPALIVE_ID_1"}),
    )];
    let actual = vec![with_fields(
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        json!({"id": "KEEPALIVE_ID_2"}),
    )];
    let report = diff(&expected, &actual);
    assert_eq!(
        report.divergence.as_ref().map(|d| d.kind),
        Some(DivergenceKind::WrongFieldValue)
    );
}

#[test]
fn diff_detects_wrong_packet() {
    let expected = vec![event(
        Direction::Clientbound,
        "play",
        39,
        "keep_alive",
        10.0,
    )];
    let actual = vec![event(
        Direction::Clientbound,
        "play",
        29,
        "kick_disconnect",
        10.0,
    )];
    let report = diff(&expected, &actual);
    assert_eq!(
        report.divergence.as_ref().map(|d| d.kind),
        Some(DivergenceKind::WrongPacket)
    );
}

#[test]
fn diff_keepalive_echo_within_strict_timing_matches() {
    // keep_alive echo has Strict timing: prompt response is fine.
    let trace = vec![
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        event(Direction::Serverbound, "play", 26, "keep_alive", 15.0),
    ];
    assert!(diff(&trace, &trace).is_match());
}

#[test]
fn diff_keepalive_echo_too_late_is_timing_violation() {
    let expected = vec![
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        event(Direction::Serverbound, "play", 26, "keep_alive", 15.0),
    ];
    let actual = vec![
        event(Direction::Clientbound, "play", 39, "keep_alive", 10.0),
        event(Direction::Serverbound, "play", 26, "keep_alive", 5000.0),
    ];
    let report = diff(&expected, &actual);
    assert_eq!(
        report.divergence.as_ref().map(|d| d.kind),
        Some(DivergenceKind::TimingViolation)
    );
}
