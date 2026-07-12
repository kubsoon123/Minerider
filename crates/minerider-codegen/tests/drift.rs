//! Drift gate: regenerating from the vendored minecraft-data must produce
//! byte-identical files to the ones committed in `minerider-protocol`.
//! CI runs `cargo test -p minerider-codegen`; a drift here means someone
//! edited generated files by hand or forgot to commit a regeneration.

use std::path::Path;

use minerider_codegen::{emit, ir::Ir, load};

fn regenerate() -> Vec<emit::GeneratedFile> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (_version, file) =
        load(&manifest.join("vendor/minecraft-data/pc/1.21.4")).expect("vendored data loads");
    let ir = Ir::build(&file).expect("ir builds");
    let (files, _stats) = emit::generate(&ir).expect("emission succeeds");
    files
}

#[test]
fn generated_files_are_up_to_date() {
    let files = regenerate();
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../minerider-protocol/src/generated");
    let mut drift = Vec::new();
    for f in &files {
        let path = out.join(&f.path);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == f.contents => {}
            Ok(_) => drift.push(f.path.clone()),
            Err(_) => drift.push(format!("{} (missing)", f.path)),
        }
    }
    assert!(
        drift.is_empty(),
        "generated files drifted — run `cargo run -p minerider-codegen` and commit: {drift:?}"
    );
}

#[test]
fn generation_is_deterministic() {
    let a = regenerate();
    let b = regenerate();
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.path, y.path);
        assert_eq!(
            x.contents, y.contents,
            "nondeterministic output in {}",
            x.path
        );
    }
}
