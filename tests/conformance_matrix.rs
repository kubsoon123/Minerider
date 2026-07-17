//! Conformance matrix tests: every clientbound packet id has a coverage
//! entry, and the committed document matches a fresh generation (drift gate).

use minerider::core::state::ConnectionState;
use minerider::minecraft::coverage::{clientbound_coverage, CoverageClass};
use minerider_protocol::generated::v1_21_4::{configuration, login, play};

#[test]
fn every_clientbound_id_is_classified() {
    for (state, ids) in [
        (ConnectionState::Login, login::CLIENTBOUND_IDS),
        (
            ConnectionState::Configuration,
            configuration::CLIENTBOUND_IDS,
        ),
        (ConnectionState::Play, play::CLIENTBOUND_IDS),
    ] {
        for &id in ids {
            let entry = clientbound_coverage(state, id);
            // The classification must be a deliberate choice, not a panic
            // or a hole; all four classes are valid answers.
            match entry.class {
                CoverageClass::Handled
                | CoverageClass::IntentionallyIgnored
                | CoverageClass::StoredForLater
                | CoverageClass::Unsupported => {}
            }
        }
    }
}

#[test]
fn conformance_doc_is_up_to_date() {
    let rendered = minerider::minecraft::coverage::render_matrix();
    // Git may materialize text files with CRLF on Windows even though the
    // deterministic renderer deliberately emits LF. Compare logical content
    // so this drift gate catches real changes instead of platform newlines.
    let committed = std::fs::read_to_string("docs/vanilla_conformance_1_21_4.md")
        .expect("read conformance matrix doc")
        .replace("\r\n", "\n");
    assert_eq!(
        committed, rendered,
        "docs/vanilla_conformance_1_21_4.md is stale; run `cargo run --bin conformance_matrix`"
    );
}
