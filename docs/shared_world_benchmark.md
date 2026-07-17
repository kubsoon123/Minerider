# Shared world/chunk memory audit and benchmark

## Status and branch relationship

This milestone starts on \`perf/shared-chunk-store\` at
\`9df9f50f68a5c6263aa249c78956320cc733da78\`. That commit is the head of
the still-unmerged draft PR #1 (\`feat/client-presentation-state\` → \`main\`),
which contains Phases 4c–4h. There is no Phase 4i branch or pull request.
This optimization branch is therefore intentionally stacked on PR #1 and
must not be merged before its base.

The baseline CI run is
[run #42](https://github.com/kubsoon123/Minerider/actions/runs/29588424074):
format, \`git diff --check\`, workspace build, Linux and Windows workspace
tests, Clippy with warnings denied, and protocol-codegen drift all pass.
The Linux workspace test job reports exactly 339 passed tests and zero
failures. Existing conformance remains \`PARTIAL\`: mock, golden and local
server evidence exists, but a real vanilla-client reference trace is still
pending.

## S1 — verified original ownership architecture

The audit below describes the code at the starting commit, before any shared
store exists.

| Lifecycle stage | Ownership and allocation |
|---|---|
| Socket receive | Each \`Connection\` owns an 8 KiB stack read buffer and a per-connection \`BytesMut\` frame buffer. Encryption mutates newly received bytes in place; decompression/\`FrameCodec\` produces one per-packet \`RawPacket\`. |
| Generated packet decode | \`PacketMapChunk::decode\` allocates owned \`Vec\` values for \`chunk_data\`, block entities, light masks and nested light arrays, plus owned NBT heightmaps. These allocations are temporary to the packet handler. |
| World conversion | \`Chunk::decode\` parses \`chunk_data\` again into a new \`Vec<ChunkSection>\`. Every indirect/direct block-state or biome container owns its own palette/data vectors. |
| Retained storage | Each play session owns one \`World\`; each \`World\` owns one \`HashMap<(i32, i32), Chunk>\`. \`Chunk\` and all section palette/data vectors are deep-owned per client. There is no \`Arc\` in this path. |
| Physics/read path | \`World::block_state\` borrows the per-client map and decodes one packed value. Collision queries borrow it repeatedly; they do not clone chunks. |
| Block updates | A single- or multi-block update mutably borrows only that client's chunk. The touched block container is fully materialized to 4096 values and repacked into a new direct container. |
| Light/block-entity updates | Initial heightmaps, block entities and light data are decoded into \`PacketMapChunk\` but not copied into the retained \`Chunk\`. Standalone \`update_light\` is not applied to \`World\`. These are current correctness/coverage gaps, not hidden shared state. |
| Unload | \`unload_chunk\` removes the client's owned \`Chunk\`; all nested vectors are dropped immediately for that client. |
| Login/respawn/dimension | Play login and every respawn replace \`World\` with a fresh empty value built from the selected \`DimensionType\`, dropping all old chunks. |
| Disconnect/reconnect | \`Client\` represents one session. Dropping/ending its play loop drops that session's world. \`ClientSupervisor\` creates a new \`Client\` and increments its generation; no world object crosses the reconnect boundary. |

### Ownership map

| Scope | Data |
|---|---|
| Once per process | Generated protocol/collision tables and the lazily initialized sine table. |
| Once per server/world | Nothing in the retained world path at baseline. |
| Once per client/session | Connection buffers, \`PlayState\`, \`World\`, dimension value, chunk-position map. |
| Once per client/chunk | \`Chunk\`, section vector, block/biome palettes and packed arrays. |
| Temporarily per map-chunk packet | Raw framed payload, generated \`PacketMapChunk\`, NBT, block-entity and light vectors, then section-decoder allocations. |

### Clone and snapshot audit

- \`PlayState::snapshot\` intentionally excludes \`World\`. Tick publication,
  packet publication and supervisor state relay therefore never deep-clone
  chunk data.
- \`BotEvent\` has no chunk payload. Event broadcast does not clone chunks.
- Physics and controller paths borrow \`&World\`; lookups allocate no chunk
  copies.
- \`src/bin/swarm.rs\` starts an independent \`ClientConfig\` and \`Client\` per
  bot. Its own module comment explicitly confirms that every bot currently
  owns a separate world/chunk cache.
- \`ClientSupervisor\` resets its public \`StateSnapshot\` on disconnect, but
  snapshots never contain the world. Reconnect creates a new play state.
- \`PacketMapChunk\` temporarily duplicates encoded chunk payload data while
  \`Chunk::decode\` constructs retained decoded sections. The packet and its
  omitted height/light/entity fields are dropped after the handler returns.

The opportunity is therefore real and narrow: clients observing identical
coordinates retain deep duplicate section vectors, while public state/event
publication does not contribute to that duplication.

## S2 — reproducible baseline harness

\`src/bin/shared_world_benchmark.rs\` models the exact retained baseline type:
one \`HashMap<(i32, i32), Chunk>\` per client, with 24 overworld-height
sections, a mix of empty sections and deterministic indirect block/biome
palettes, and deep cloning between clients.

Run an optimized suite with:

\`\`\`text
cargo run --release --bin shared_world_benchmark
\`\`\`

The suite runs each workload in a fresh child process, performs one exact
warm-up and five measured constructions, and reports median/min/max time.
It covers 1, 10 and 100 clients; identical, approximately 98%-identical and
personalized views; update and lifecycle churn; a measured 49-chunk reduced
view; and one measured 441-chunk client. The pre-optimization 100 × 441
logical retained size is extrapolated from the measured one-client fixture
instead of allocating several GiB in CI.

Deterministic \`logical_retained_bytes\` is a documented lower-bound model:
map capacity multiplied by the inline key/value size plus the exact
capacities of section, palette and packed-data allocations. It excludes
allocator metadata and \`HashMap\` control bytes. Linux-only \`VmRSS\`/\`VmHWM\`
readings come from \`/proc/self/status\`; other platforms print
\`unavailable\`. RSS, logical bytes and extrapolations are never conflated.

This first commit establishes the audit and baseline harness only. Results
and any justified production design are added after the release benchmark
runs on CI.
