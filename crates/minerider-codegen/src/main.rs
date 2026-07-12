//! Generator binary: regenerates `minerider-protocol/src/generated/` from
//! vendored minecraft-data.
//!
//! Usage:
//!   cargo run -p minerider-codegen            regenerate files
//!   cargo run -p minerider-codegen -- --check verify committed files are up to date (drift gate)

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use minerider_codegen::{emit, ir::Ir, load};

fn vendor_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/minecraft-data/pc/1.21.4")
}

fn output_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../minerider-protocol/src/generated")
}

fn run() -> Result<ExitCode, String> {
    let check = std::env::args().any(|a| a == "--check");

    let (_version, file) = load(&vendor_dir()).map_err(|e| format!("load failed: {e}"))?;
    let ir = Ir::build(&file).map_err(|e| format!("ir build failed: {e}"))?;
    let (files, stats) = emit::generate(&ir).map_err(|e| format!("emit failed: {e}"))?;

    let out = output_dir();
    if check {
        let mut drift = Vec::new();
        for f in &files {
            let path = out.join(&f.path);
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing == f.contents => {}
                Ok(_) => drift.push(f.path.clone()),
                Err(_) => drift.push(format!("{} (missing)", f.path)),
            }
        }
        if drift.is_empty() {
            println!("drift check OK: {} generated files up to date", files.len());
            return Ok(ExitCode::SUCCESS);
        }
        for d in &drift {
            eprintln!("DRIFT: {d}");
        }
        eprintln!(
            "{} generated file(s) differ — run `cargo run -p minerider-codegen` and commit",
            drift.len()
        );
        return Ok(ExitCode::FAILURE);
    }

    // Write all files; remove stale generated files not in the current set.
    let mut current = std::collections::BTreeSet::new();
    for f in &files {
        let path = out.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &f.contents).map_err(|e| format!("write {}: {e}", path.display()))?;
        current.insert(f.path.replace('\\', "/"));
    }
    for entry in walk_rs(&out) {
        let rel = entry
            .strip_prefix(&out)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if !current.contains(&rel) {
            std::fs::remove_file(&entry).map_err(|e| format!("remove {}: {e}", entry.display()))?;
            println!("removed stale {}", rel);
        }
    }

    let total_packets: usize = stats.states.iter().map(|s| s.2 + s.3).sum();
    println!(
        "generated {} files ({} packets, {} shared types) into {}",
        files.len(),
        total_packets,
        stats.shared_types,
        out.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_rs(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
