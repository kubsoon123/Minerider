# MineRider Architecture

## Crate layout

```text
crates/
  minerider-protocol/     pure wire protocol — no game logic, no async runtime
      ↑ depends on
  minerider/ (root)       async network + game-state machines
      ↑ will depend on
  minerider-lua/          (phase 5) Lua embedding and bot API
```

`minerider-protocol` is a standalone library: it never imports tokio or
anything from the client crate, so it can be reused by tooling (proxies,
packet dumpers, the future data generator) and fuzzed in isolation. The
`minerider` crate depends on it by path; shared dependency versions live in
`[workspace.dependencies]` in the root `Cargo.toml`.

## Module responsibilities

### minerider-protocol

| Module | Responsibility |
|---|---|
| `error` | `ProtocolError` (thiserror): `VarIntTooLong`, `VarLongTooLong`, `BufferUnderflow`, `NegativeLength`, `InvalidString`, `InvalidUtf8`, `InvalidIdentifier`, `FrameTooLarge`, `Compression`, `Crypto` + `Result` alias |
| `varint` / `varlong` | Minecraft VarInt (≤5 bytes) / VarLong (≤10 bytes) encode/decode/size |
| `buffer` | `PacketWriter` / `PacketReader`: primitives, strings (32 767 char cap), identifiers, UUIDs, positions, options, arrays; all reads bounds-checked, byte slices and strings lent zero-copy |
| `packet` | `RawPacket { id, payload }` |
| `codec` | `FrameCodec`: VarInt length-prefix framing, optional zlib compression, 2 MiB frame cap |
| `compression` | zlib helpers with exact-length decompression and a 64 MiB zip-bomb cap |
| `crypto::rsa` | PKCS#1 v1.5 DER public-key encryption (login exchange) |
| `crypto::aes` | `StreamCipher`: AES-128-CFB8, key = IV = shared secret |

### minerider

| Module | Responsibility |
|---|---|
| `network::tcp` | `TcpTransport`: connect with timeout, split halves, TCP_NODELAY |
| `network::connection` | `Connection`: owns socket + `FrameCodec` + optional `StreamCipher`; decrypts fresh bytes, then deframes; 8 KiB reads, 30 s default read timeout |
| `minecraft::handshake` | Handshake packet struct + send helper |
| `minecraft::login` | Login state machine: Login Start, Encryption Request/Response, Set Compression, Login Success, plugin-request refusal |
| `minecraft::configuration` | Configuration state machine: keep-alive/ping echo, known packs, Finish Configuration |
| `minecraft::play` | Minimal play loop: keep-alive echo, disconnect handling |
| `core::client` | `ClientConfig` / `Client`: connect → handshake → login → configuration → play |
| `core::state` | `ConnectionState` enum |
| `core::error` | `MineRiderError`: `Wire` wraps `ProtocolError` via `#[from]`; `Protocol(String)` is reserved for game-level violations (unexpected packet ids, broken sequences) |
| `core::tick` | Stub for the phase-3 tick engine |

## Connection state machine

```text
Handshaking ──handshake(next=2)──▶ Login ──Login Success──▶ Configuration ──Finish──▶ Play
     │
     └──handshake(next=1)──▶ Status (server-list ping; not used by the client)
```

Every transition is explicit (`Connection::set_state`); the state is
observable via `Connection::state()` / `Client::state()`.

## Wire byte order: encryption, framing, compression

Outbound (client → server), per packet:

```text
packet id + payload
  │ 1. compress (if compression enabled and body ≥ threshold)
  ▼
VarInt frame_len | [VarInt data_length] | body
  │ 2. encrypt whole frame (if encryption enabled)
  ▼
TCP
```

Inbound processing is the exact reverse — **decrypt → deframe → decompress
→ parse**:

1. **Decrypt**: AES-CFB8 applies to every byte after the Encryption
   Response, including frame-length VarInts, so fresh socket bytes are
   decrypted in place before anything looks at them
   (`network/connection.rs`, `Connection::read_packet`).
2. **Deframe**: peek the frame-length VarInt (reject > 2 MiB or > 5-byte
   VarInts), wait until the full frame is buffered, split it off.
3. **Decompress**: if compression is enabled, read `data_length`; `0` means
   the body is raw, otherwise inflate to exactly `data_length` bytes.
4. **Parse**: first VarInt of the body is the packet id, the rest is the
   payload handed to the state machines.

## Performance notes

- **AES-CFB8 is latency-bound, not throughput-bound.** CFB8 with a 16-byte
  shift register needs one AES block encryption per byte, and each byte's
  input depends on the previous byte's output — the chain is serial. On the
  benchmark machine one byte costs ~16 ns ≈ one AES-NI 10-round latency, so
  ~62 MiB/s is the hardware bound for this mode; no API change can beat it.
  Typical frames (tens to hundreds of bytes) cost microseconds.
- **Bulk cipher call.** The phase-1 code called `encrypt_block_mut` per
  byte, believing cipher-0.4's `AsyncStreamCipher` consumes `self` (it
  does — verified in `cipher-0.4.4/src/stream.rs`). The state-preserving
  alternative `encrypt_blocks_mut` needs `&mut [Block]`, which
  generic-array 0.14 cannot safely produce from `&mut [u8]`. The cipher
  therefore runs the ~10-line CFB8 feedback loop directly on RustCrypto's
  `Aes128Enc` block primitive (AES itself is still library code);
  correctness is pinned by an `openssl enc -aes-128-cfb8` known-answer
  vector. Measured throughput is parity with the old path (~8 %, within
  noise) because LLVM had inlined the per-byte dispatch — the fix is about
  a correct, single-bulk-call API, not a speedup.
- **Zero-copy reads.** `PacketReader::read_bytes` / `read_byte_array` /
  `read_string` lend `&[u8]` / `&str` from the underlying buffer. The codec
  borrows frame bytes (`Cow`) when no decompression is needed, so the
  uncompressed path copies exactly once (into the owned payload).
- **No per-read allocation.** `Connection::read_packet` decrypts the fresh
  bytes in place in the 8 KiB stack buffer, then extends the read buffer;
  the old `chunk[..n].to_vec()` per read is gone.
- **No `unwrap`/`expect` outside tests.** Fixed-width reads go through a
  `take::<N>()` helper using `copy_from_slice` after the bounds check.

## Benchmarks

Machine: **11th Gen Intel Core i5-11400F @ 2.60 GHz**, Windows, release
build (criterion, 20 samples, 2 s measurement). Median values.

| Benchmark | Time | Throughput |
|---|---|---|
| varint encode (1024 mixed-size values) | 3.91 µs | 976 MiB/s |
| varint decode (1024 mixed-size values) | 3.30 µs | 1.13 GiB/s |
| frame encode 1 KiB raw | 102 ns | 9.33 GiB/s |
| frame decode 1 KiB raw | 141 ns | 6.76 GiB/s |
| frame encode 64 KiB raw | 4.05 µs | 15.1 GiB/s |
| frame decode 64 KiB raw | 3.96 µs | 15.4 GiB/s |
| frame encode 1 KiB compressed (repetitive) | 12.0 µs | 81.6 MiB/s |
| frame decode 1 KiB compressed (repetitive) | 4.66 µs | 210 MiB/s |
| frame encode 64 KiB compressed (random) | 119.9 µs | 521 MiB/s |
| frame decode 64 KiB compressed (random) | 28.8 µs | 2.12 GiB/s |
| AES-CFB8 encrypt bulk 1 MiB | 15.9 ms | 62.5 MiB/s |
| AES-CFB8 decrypt bulk 1 MiB | 15.9 ms | 62.9 MiB/s |
| AES-CFB8 encrypt 64 KiB, legacy per-byte path | 920 µs | 68.0 MiB/s |

Run with `cargo bench -p minerider-protocol`.

## Testing strategy

- **Unit tests (minerider-protocol)**: byte-exact vanilla vectors
  (VarInt/VarLong, Position, UUID byte order), string/identifier/option/
  array boundaries, compression threshold boundary, malformed and
  truncated inputs, AES known-answer vector, RSA roundtrip and tampered
  ciphertext. Fuzz-style deterministic PRNG sweeps for varint roundtrips.
- **Integration tests (minerider)**: a `MockServer` speaking the full
  phase-1 flow (encrypted and plain variants), plus raw-socket stream
  tests: one-byte dribbles, frames split across segments, multiple frames
  per segment, malformed VarInts mid-stream, oversized frames, garbage
  input (must error, never panic/hang), clean shutdown, reconnect, and a
  silent-peer timeout.
- **Rule**: malformed input must produce a typed error — never a panic,
  never a hang (bounded by per-read timeouts).
