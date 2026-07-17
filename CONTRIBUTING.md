# Contributing to MineRider

## Acceptable use

MineRider is for authorized use: your own Minecraft servers, or servers whose
operator has explicitly agreed to bots connecting for testing, monitoring, or
automation. Contributions that primarily serve anti-cheat bypassing, AFK-kick
evasion, ban evasion, proxy rotation, chat spam, griefing, or account farming
will not be accepted, regardless of how they're framed. If a change only
makes sense as a way to hide bot activity from server administrators, it does
not belong in this project.

## Before you open a PR

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p minerider-codegen -- --check
```

All four must pass. CI runs the same checks.

## Generated files — never hand-edit

Two things in this repository are generated and must only be changed by
re-running their generator:

- `crates/minerider-protocol/src/generated/**` — regenerate with
  `cargo run -p minerider-codegen`. The drift gate
  (`cargo run -p minerider-codegen -- --check`, also exercised by
  `crates/minerider-codegen/tests/drift.rs`) fails the build if the committed
  output doesn't match what the generator produces from the vendored
  `minecraft-data` — including from an editor's "format on save" silently
  rewriting a generated file.
- `src/minecraft/collision_data.rs` — regenerate with
  `python scripts/generate_collision_data.py` from the vendored
  `blocks.json` / `blockCollisionShapes.json`. This file is intentionally
  kept in a compact non-default rustfmt style (see the `#[rustfmt::skip]`
  attributes the generator emits) because it holds ~28k lines of table data;
  `cargo fmt --all --check` will pass on it as generated, so if you ever see
  a diff there, regenerate rather than hand-format.

If you need to change what either generator produces (a new field, a new
friction override, ...), change the generator, not its output, then commit
both.

## Testing conventions

- Prefer real integration tests (a real socket against `tests/common`'s mock
  server) over mocking at a low level — this project has been burned before
  by tests that were self-consistent with a wrong assumption and couldn't
  catch real wire-format drift (see `docs/engineering_review.md`, §1).
- Add a regression test for every bug fix.
- Live-server and live-Microsoft-account tests are not part of the default
  suite and must stay that way; gate anything that needs real network access
  or real credentials behind an environment variable, and document it.
- Don't weaken an existing security bound (frame size caps, allocation
  guards, RSA key-size bounds, etc.) to make a test pass.

## Code style

- No `unwrap`/`expect`/`panic!` on paths that process untrusted (server or
  network) input. Structured errors only.
- Keep wire protocol logic inside `minerider-protocol`; keep game/world logic
  out of it. No `tokio` in `minerider-protocol`.
- Don't add a dependency the standard library or an existing dependency
  already solves cleanly.
- Match the existing doc-comment style: explain *why*, not just *what*;
  avoid restating the code.

## Alpha release checklist

Before tagging a release:

- [ ] `cargo fmt --all --check`, `cargo test --workspace`,
      `cargo clippy --workspace --all-targets -- -D warnings`, and
      `cargo run -p minerider-codegen -- --check` all pass.
- [ ] `docs/progress.md` reflects the actual state of the code (no
      undocumented features, no documented-but-removed features).
- [ ] README claims match verified behavior — no unverified performance
      numbers, no features described as done that are actually partial.
- [ ] No secrets, tokens, session caches, or local server artifacts are
      tracked in git (`git status --short --untracked-files=all` should show
      nothing sensitive; see `.gitignore`).
- [ ] `LICENSE-MIT`, `LICENSE-APACHE`, and `THIRD_PARTY_NOTICES.md` are
      present and accurate.
