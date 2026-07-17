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

## S2 baseline results

Baseline measurements come from GitHub Actions Ubuntu x86_64, Rust
\`1.97.1 (8bab26f4f 2026-07-14)\`, release profile with thin LTO, 24
sections per chunk, one exact warm-up and five construction samples. CI
[run #45](https://github.com/kubsoon123/Minerider/actions/runs/29595300932)
passed all seven jobs, including the release benchmark.

| Scenario | Clients | Chunks/client | Deep payload copies | Unique contents | Logical retained bytes | Retained RSS | Median construction |
|---|---:|---:|---:|---:|---:|---:|---:|
| identical | 1 | 49 | 49 | 49 | 4,721,920 | 11,620 KiB | 6,570 µs |
| identical | 10 | 49 | 490 | 49 | 47,219,200 | 53,684 KiB | 21,173 µs |
| identical | 100 | 49 | 4,900 | 49 | 472,192,000 | 474,316 KiB | 199,190 µs |
| ~98% identical | 100 | 49 | 4,900 | 149 | 472,192,000 | 474,316 KiB | 197,097 µs |
| personalized | 100 | 49 | 4,900 | 4,900 | 472,192,000 | 474,752 KiB | 541,641 µs |
| update churn | 100 | 49 | 4,900 | 49 before updates | 472,192,000 | 474,320 KiB | 194,939 µs |
| lifecycle churn | 100 | 49 | 4,900 | 49 before churn | 472,192,000 | 474,316 KiB | 195,493 µs |
| identical full view | 1 | 441 | 441 | 441 | 42,495,040 | 86,420 KiB | 70,185 µs |

The 100-client identical case increased RSS from 4,244 KiB after warm-up
to 474,316 KiB while retained. The deterministic lower bound is 450.3 MiB;
the measured RSS increase is about 459 MiB. The mostly-identical case costs
the same at baseline because ownership is per client regardless of equality.
The personalized case is intentionally the worst sharing candidate and its
construction is slower because every fixture is generated independently.

For the full 21 × 21 view, the measured one-client logical value
(42,495,040 bytes) extrapolates linearly to 4,249,504,000 bytes (about
3.96 GiB) for 100 baseline clients. That 100-client full-view number is an
estimate, not an RSS measurement; the process was deliberately not allocated
in CI.

Update churn performed 800 section updates in 19,298 µs. Lifecycle churn
performed four half-unload/reload rounds in 177,277 µs. Identical-case lookup
for 156,800 reads took 11,595 µs. RSS after drop is allocator-dependent:
most cases returned near 4–7 MiB, while the personalized and 441-chunk child
processes retained arenas. Therefore cleanup correctness is tested through
strong/weak ownership counts and deterministic logical metrics; RSS after
drop is reported but not treated as proof of a leak.

### Decision

Production interning is justified. At 100 × 49, identical clients retain
100 deep copies of every logical chunk and approximately 99% of payload bytes
are theoretically redundant. The implementation must still prove that the
personalized case remains bounded and that hashing/equality/update overhead
does not create an unreasonable CPU regression.

## S3 — correctness design

The production model keeps visibility per client and shares only immutable
payload ownership:

\`\`\`text
SharedChunkStore
  (server identity, world generation, dimension identity, position, fingerprint)
      -> bounded collision bucket of Weak<Chunk>

World (one per client)
  position -> Arc<Chunk>
\`\`\`

- A process-wide store contains only \`Weak\` references. It cannot keep a
  chunk or world alive after every client unloads it.
- The store is sharded across fixed-size mutexes. No lock is held across an
  await, and a slow key does not serialize the whole world.
- The key contains the configured server host/port, typed dimension
  properties, a monotonically changing session/respawn generation, chunk
  position and a deterministic content fingerprint.
- Fingerprints cover sections, biomes, heightmaps, block entities, light
  masks and light arrays. A matching fingerprint is only a lookup hint:
  full canonical \`PartialEq\` is required before reuse.
- Each \`World\` owns its own position index. Interning never inserts a
  coordinate into another client, so one bot cannot discover a chunk merely
  because another bot received it.
- A block update creates a new immutable chunk version and clones only the
  touched section; unchanged sections stay behind \`Arc\`. Light and block
  entity updates replace only their immutable sub-payload.
- A receiving client's map is atomically replaced after the new version is
  complete. Other clients retain the prior \`Arc\`.
- Login, respawn and reconnect create a fresh world generation and empty
  per-client index. No stale coordinate is visible without a new authoritative
  packet.
- Cache keys and forced-collision buckets are bounded. Deterministic eviction
  drops only weak lookup entries, never a client's strong reference.
- Sharing can be disabled in \`ClientConfig\`; strict content interning is the
  safe default because equality never overrides per-client visibility.

This design deliberately does not add a “trusted shared-authoritative world”
mode. Coordinates alone are never evidence that another client received the
same content.
