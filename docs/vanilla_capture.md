# Vanilla-client capture & movement parity

MineRider's movement is reconstructed from the 1.21.4 client source and pinned
by unit tests (terminal velocity, jump apex, walk/sprint/sneak speed, ice slip,
step-up). That makes the physics *self-consistent with the documented vanilla
math*, but the honest evidence ceiling for "a server cannot tell this is a bot"
is a **byte/timing diff against a real vanilla client** running the same
scenario. This document is that procedure.

> **This step needs a human.** It requires the official Minecraft client and a
> Mojang/Microsoft account, so it cannot run in CI or be executed by an agent.
> Everything below is the manual protocol for producing the reference capture
> and comparing it; the tooling to normalize and diff traces already exists in
> `src/trace/`.

## What we are proving

A server's anti-cheat re-simulates the client's movement and flags divergence in:

1. **Movement-packet cadence** — which of `position` / `position_look` / `look`
   / `flying` is sent each tick, and the 20-tick unchanged-position reminder.
2. **`on_ground` / horizontal-collision flag timing** — e.g. a resting player
   reports `on_ground` on the *second* physics tick, not the first.
3. **The position stream itself** — per-tick coordinates for walking,
   sprinting, jumping, sprint-jumping, ice, and single-step traversal must
   match vanilla to well within the server's tolerance.

## Producing the reference (vanilla) trace

1. Stand up a local server (Paper or vanilla) on `127.0.0.1:25565` in a flat or
   fixed world, offline-mode, so both clients see identical terrain.
2. Put a **logging TCP proxy** between the official client and the server
   (any mitm proxy that dumps framed packets with timestamps; the fields we
   compare are direction, tick/relative-time, packet id and decoded body).
   Point the official client at the proxy's port.
3. Log in with the real client and perform each scripted scenario below,
   keeping inputs minimal and deterministic. Save one capture per scenario.

## Producing the MineRider trace

Run the CLI against the **same server** with tracing enabled:

```sh
MINERIDER_TRACE=trace-walk.jsonl MINERIDER_TRACE_SCENARIO=walk \
  cargo run -- 127.0.0.1 25565 TraceBot
```

To exercise movement rather than idling, drive the bot through
`Client::control()` (a `ControlHandle`): `walk_to`, `look`, `sprint`, `jump`,
`set_input`. A small scenario harness that issues a fixed command timeline and
exits is the natural companion to add next to `src/bin/`.

## Scenarios to compare

| Scenario      | Vanilla input                              | MineRider control |
|---------------|--------------------------------------------|-------------------|
| idle          | stand still 3 s                            | (none)            |
| walk          | hold forward 2 s on flat ground            | `walk_to` ahead   |
| sprint        | hold sprint+forward 2 s                    | `sprint(true)` + forward |
| jump          | single jump, no input                      | `jump` one tick   |
| sprint-jump   | sprint forward + repeated jump             | sprint + `jump` held |
| ice           | walk onto ice, release, coast              | forward then stop on ice |
| step          | walk into a slab / 1-block-high step        | `walk_to` across a step |

## Diffing

Normalize both traces (redacts absolute time and symbolizes ids) and diff:

```rust
use minerider::trace::{normalize::normalize, diff::diff, recorder::read_trace};
let ours = normalize(&read_trace("trace-walk.jsonl")?);
let vanilla = normalize(&read_trace("vanilla-walk.jsonl")?);
let report = diff(&vanilla, &ours);      // vanilla is the reference
assert!(report.is_match(), "{:?}", report.divergence);
```

The `diff` comparator already tolerates run-to-run timing jitter within a strict
window and compares packet order and decoded fields, so a clean match on the
walk/jump/ice/step scenarios is what upgrades the movement evidence from
`PARTIAL` to verified.

## Known deltas to expect (and explain, not silence)

- **`Mth.sin` table:** rotation uses the exact vanilla lookup table, so cardinal
  and arbitrary yaws should match. Positions are still computed in `f64` (vanilla
  mixes `f32`/`f64`); sub-ULP differences may appear and must stay far below the
  server's movement tolerance.
- **Block friction:** only ice family, blue ice and slime differ from `0.6`.
  Honey/soul-sand slowdowns and fluids are **not** implemented yet — do not run
  those scenarios as parity checks until they are (see `docs/progress.md`).
