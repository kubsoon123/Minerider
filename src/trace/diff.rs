//! Semantic trace comparator.
//!
//! Compares two normalized traces (e.g. vanilla capture vs MineRider run)
//! and reports the *first meaningful divergence* instead of flooding the
//! report with downstream fallout. Timing is compared with tolerance
//! classes from [`crate::minecraft::coverage`], never exact equality.

use std::fmt;

use crate::core::state::ConnectionState;
use crate::minecraft::coverage::{clientbound_coverage, TimingClass};

use super::format::{Direction, TraceEvent};

/// Kind of the first divergence found between two traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    /// An expected packet never appeared.
    MissingPacket,
    /// A packet appeared that the expected trace does not have.
    ExtraPacket,
    /// Same position, but a different packet (id/state/direction).
    WrongPacket,
    /// Same packet, but a normalized field value differs.
    WrongFieldValue,
    /// Packet matched, but its response timing violates the tolerance class.
    TimingViolation,
}

impl fmt::Display for DivergenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::MissingPacket => "missing packet",
            Self::ExtraPacket => "extra packet",
            Self::WrongPacket => "wrong packet",
            Self::WrongFieldValue => "wrong field value",
            Self::TimingViolation => "timing violation",
        };
        f.write_str(s)
    }
}

/// The first meaningful divergence between two traces.
#[derive(Debug)]
pub struct Divergence {
    /// Index in the expected trace where the walk stopped.
    pub index: usize,
    pub kind: DivergenceKind,
    pub detail: String,
}

/// Result of comparing two traces.
#[derive(Debug)]
pub struct DiffReport {
    /// Number of events matched before the divergence (or total if equal).
    pub compared: usize,
    /// First meaningful divergence, if any.
    pub divergence: Option<Divergence>,
    /// Estimated number of events affected downstream of the divergence.
    /// Grouped, not listed: one root cause, one report.
    pub fallout: usize,
}

impl DiffReport {
    pub fn is_match(&self) -> bool {
        self.divergence.is_none()
    }
}

/// How far ahead the resync window looks when classifying a mismatch.
const LOOKAHEAD: usize = 8;

fn event_label(event: &TraceEvent) -> String {
    format!(
        "{:?} {} id={} ({})",
        event.dir, event.state, event.id, event.name
    )
}

/// Structural equality: direction, state, id, name and normalized fields.
fn matches(expected: &TraceEvent, actual: &TraceEvent) -> bool {
    expected.dir == actual.dir
        && expected.state == actual.state
        && expected.id == actual.id
        && expected.name == actual.name
        && expected.fields == actual.fields
}

fn same_packet(a: &TraceEvent, b: &TraceEvent) -> bool {
    a.dir == b.dir && a.state == b.state && a.id == b.id && a.name == b.name
}

fn find_ahead(haystack: &[TraceEvent], from: usize, needle: &TraceEvent) -> Option<usize> {
    haystack
        .iter()
        .skip(from)
        .take(LOOKAHEAD)
        .position(|event| matches(event, needle))
        .map(|pos| from + pos)
}

fn state_of(event: &TraceEvent) -> Option<ConnectionState> {
    match event.state.as_str() {
        "login" => Some(ConnectionState::Login),
        "configuration" => Some(ConnectionState::Configuration),
        "play" => Some(ConnectionState::Play),
        _ => None,
    }
}

/// Timing tolerance for a serverbound response, derived from the coverage
/// obligation of the clientbound packet that triggered it.
fn timing_ok(class: TimingClass, delay_expected_ms: f64, delay_actual_ms: f64) -> bool {
    match class {
        // Strict responses (acks) must be prompt; allow generous absolute
        // slack for scheduler jitter but nothing queue-like.
        TimingClass::Strict => {
            delay_actual_ms <= delay_expected_ms + 100.0 && delay_actual_ms <= 1000.0
        }
        // Tick-bound: within two ticks of the expected delay.
        TimingClass::TickBound => (delay_actual_ms - delay_expected_ms).abs() <= 100.0,
        // Periodic: interval ratio within a factor of two.
        TimingClass::Periodic => {
            delay_expected_ms <= f64::EPSILON
                || (delay_actual_ms / delay_expected_ms).clamp(0.0, f64::MAX) >= 0.5
                    && delay_actual_ms <= delay_expected_ms * 2.0 + 50.0
        }
        TimingClass::BestEffort | TimingClass::None => true,
    }
}

/// Compares two normalized traces. Both traces should be produced by the
/// same scenario in the same environment.
pub fn diff(expected: &[TraceEvent], actual: &[TraceEvent]) -> DiffReport {
    let mut i = 0;
    let mut j = 0;
    let mut compared = 0;

    let divergence = loop {
        if i >= expected.len() && j >= actual.len() {
            break None;
        }
        if i >= expected.len() {
            break Some(Divergence {
                index: i,
                kind: DivergenceKind::ExtraPacket,
                detail: format!("unexpected {}", event_label(&actual[j])),
            });
        }
        if j >= actual.len() {
            break Some(Divergence {
                index: i,
                kind: DivergenceKind::MissingPacket,
                detail: format!("expected {}", event_label(&expected[i])),
            });
        }

        if matches(&expected[i], &actual[j]) {
            if let Some(violation) = check_timing(expected, actual, i, j) {
                break Some(violation);
            }
            i += 1;
            j += 1;
            compared += 1;
            continue;
        }

        // Same packet at the same position, but a field differs.
        if same_packet(&expected[i], &actual[j]) {
            break Some(Divergence {
                index: i,
                kind: DivergenceKind::WrongFieldValue,
                detail: format!(
                    "{} fields differ: expected {:?}, got {:?}",
                    event_label(&expected[i]),
                    expected[i].fields,
                    actual[j].fields
                ),
            });
        }

        // Resync window: did the expected packet show up later in actual?
        if let Some(found) = find_ahead(actual, j + 1, &expected[i]) {
            break Some(Divergence {
                index: i,
                kind: DivergenceKind::ExtraPacket,
                detail: format!(
                    "unexpected {} (expected {} reappears {} events later)",
                    event_label(&actual[j]),
                    event_label(&expected[i]),
                    found - j
                ),
            });
        }
        // Or did an actual packet correspond to a later expected one?
        if find_ahead(expected, i + 1, &actual[j]).is_some() {
            break Some(Divergence {
                index: i,
                kind: DivergenceKind::MissingPacket,
                detail: format!(
                    "expected {} before {}",
                    event_label(&expected[i]),
                    event_label(&actual[j])
                ),
            });
        }

        break Some(Divergence {
            index: i,
            kind: DivergenceKind::WrongPacket,
            detail: format!(
                "expected {}, got {}",
                event_label(&expected[i]),
                event_label(&actual[j])
            ),
        });
    };

    let fallout = if divergence.is_some() {
        (expected.len() - i).max(actual.len() - j).saturating_sub(1)
    } else {
        0
    };

    DiffReport {
        compared,
        divergence,
        fallout,
    }
}

/// Checks the timing class of a serverbound response against the coverage
/// obligation of the most recent clientbound packet in the expected trace.
fn check_timing(
    expected: &[TraceEvent],
    actual: &[TraceEvent],
    i: usize,
    j: usize,
) -> Option<Divergence> {
    if expected[i].dir != Direction::Serverbound {
        return None;
    }
    let trigger = expected[..i]
        .iter()
        .rev()
        .find(|event| event.dir == Direction::Clientbound)?;
    let state = state_of(trigger)?;
    let class = clientbound_coverage(state, trigger.id).obligation.timing;
    if class == TimingClass::None || class == TimingClass::BestEffort {
        return None;
    }
    let delay_expected = expected[i].ts_rel_ms - trigger.ts_rel_ms;
    // The actual trigger is the most recent clientbound before j as well.
    let actual_trigger = actual[..j]
        .iter()
        .rev()
        .find(|event| event.dir == Direction::Clientbound)?;
    let delay_actual = actual[j].ts_rel_ms - actual_trigger.ts_rel_ms;
    if timing_ok(class, delay_expected, delay_actual) {
        return None;
    }
    Some(Divergence {
        index: i,
        kind: DivergenceKind::TimingViolation,
        detail: format!(
            "{} responded after {:.1}ms (expected {:.1}ms, class {:?})",
            event_label(&actual[j]),
            delay_actual,
            delay_expected,
            class
        ),
    })
}
