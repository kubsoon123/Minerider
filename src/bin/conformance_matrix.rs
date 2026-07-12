//! Generates `docs/vanilla_conformance_1_21_4.md` from the live coverage
//! table and the generated 1.21.4 packet registries.
//!
//! Run: `cargo run --bin conformance_matrix`
//! Drift check: `cargo run --bin conformance_matrix -- --check`
//! exits non-zero if the committed document differs from a fresh generation.

use minerider::minecraft::coverage::render_matrix;

const DOC_PATH: &str = "docs/vanilla_conformance_1_21_4.md";

fn main() {
    let rendered = render_matrix();
    let check = std::env::args().any(|arg| arg == "--check");
    if check {
        match std::fs::read_to_string(DOC_PATH) {
            Ok(existing) if existing == rendered => {
                println!("{DOC_PATH} is up to date");
            }
            Ok(_) => {
                eprintln!("{DOC_PATH} is stale; run `cargo run --bin conformance_matrix`");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("cannot read {DOC_PATH}: {err}");
                std::process::exit(1);
            }
        }
    } else {
        std::fs::write(DOC_PATH, rendered).expect("write conformance matrix");
        println!("wrote {DOC_PATH}");
    }
}
