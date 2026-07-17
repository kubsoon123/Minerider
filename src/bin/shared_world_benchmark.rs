//! Deterministic retained-world benchmark for the current per-client chunk
//! ownership model. Run with:
//!
//! cargo run --release --bin shared_world_benchmark
//!
//! Each case runs in a fresh child process so Linux RSS/HWM readings do not
//! inherit allocator high-water marks from a previous workload.

use std::collections::HashMap;
use std::hint::black_box;
use std::mem::size_of;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::{Duration, Instant};

use minerider::minecraft::configuration::DimensionType;
use minerider::minecraft::shared_world::{
    ChunkLight, ChunkSnapshot, ServerIdentity, SharedChunkStore, WorldScope,
};
use minerider::minecraft::world::{Chunk, ChunkSection, PalettedContainer};
use minerider_protocol::nbt::Nbt;

const SECTION_COUNT: usize = 24;
const BLOCKS_PER_SECTION: usize = 16 * 16 * 16;
const DEFAULT_CHUNKS: usize = 49;
const DEFAULT_SAMPLES: usize = 5;
const UPDATE_ROUNDS: usize = 8;

type ChunkMap = HashMap<(i32, i32), Chunk>;
type SharedChunkMap = HashMap<(i32, i32), Arc<ChunkSnapshot>>;

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Identical,
    MostlyIdentical,
    Personalized,
    UpdateChurn,
    LifecycleChurn,
}

impl Scenario {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "identical" => Some(Self::Identical),
            "mostly-identical" => Some(Self::MostlyIdentical),
            "personalized" => Some(Self::Personalized),
            "update-churn" => Some(Self::UpdateChurn),
            "lifecycle-churn" => Some(Self::LifecycleChurn),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Identical => "identical",
            Self::MostlyIdentical => "mostly-identical",
            Self::Personalized => "personalized",
            Self::UpdateChurn => "update-churn",
            Self::LifecycleChurn => "lifecycle-churn",
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args
        .get(1)
        .is_some_and(|arg| arg == "--case" || arg == "--shared-case")
    {
        return run_child(&args);
    }
    run_suite()
}

fn run_suite() -> ExitCode {
    let rustc = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unavailable".to_owned());
    println!(
        "ENV,os={},arch={},rustc={rustc},sections_per_chunk={SECTION_COUNT},samples={DEFAULT_SAMPLES}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "NOTE,logical_bytes are deterministic lower-bound retained-size estimates; rss fields are Linux /proc measurements when available"
    );

    let cases = [
        (Scenario::Identical, 1, DEFAULT_CHUNKS),
        (Scenario::Identical, 10, DEFAULT_CHUNKS),
        (Scenario::Identical, 100, DEFAULT_CHUNKS),
        (Scenario::MostlyIdentical, 100, DEFAULT_CHUNKS),
        (Scenario::Personalized, 100, DEFAULT_CHUNKS),
        (Scenario::UpdateChurn, 100, DEFAULT_CHUNKS),
        (Scenario::LifecycleChurn, 100, DEFAULT_CHUNKS),
        // A full 21x21 view is measured for one client. The 100-client
        // pre-optimization equivalent is intentionally extrapolated from
        // logical bytes instead of allocating several GiB in CI.
        (Scenario::Identical, 1, 441),
    ];
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("failed to locate benchmark executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    for (scenario, clients, chunks) in cases {
        let clients_arg = clients.to_string();
        let chunks_arg = chunks.to_string();
        let samples_arg = DEFAULT_SAMPLES.to_string();
        for flag in ["--case", "--shared-case"] {
            let output = match Command::new(&exe)
                .args([
                    flag,
                    scenario.name(),
                    &clients_arg,
                    &chunks_arg,
                    &samples_arg,
                ])
                .output()
            {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("failed to run {} case: {error}", scenario.name());
                    return ExitCode::FAILURE;
                }
            };
            print!("{}", String::from_utf8_lossy(&output.stdout));
            if !output.status.success() {
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn run_child(args: &[String]) -> ExitCode {
    let Some(scenario) = args.get(2).and_then(|value| Scenario::parse(value)) else {
        eprintln!("unknown or missing scenario");
        return ExitCode::FAILURE;
    };
    let Some(clients) = parse_positive(args.get(3)) else {
        eprintln!("clients must be a positive integer");
        return ExitCode::FAILURE;
    };
    let Some(chunks) = parse_positive(args.get(4)) else {
        eprintln!("chunks must be a positive integer");
        return ExitCode::FAILURE;
    };
    let Some(samples) = parse_positive(args.get(5)) else {
        eprintln!("samples must be a positive integer");
        return ExitCode::FAILURE;
    };

    if args.get(1).is_some_and(|arg| arg == "--shared-case") {
        return run_shared_child(scenario, clients, chunks, samples);
    }

    // Exact-case warm-up, dropped before collecting samples.
    drop(build_worlds(scenario, clients, chunks));

    let rss_baseline = linux_memory_kib("VmRSS");
    let mut construction = Vec::with_capacity(samples);
    let mut worlds = Vec::new();
    for sample in 0..samples {
        let start = Instant::now();
        let built = build_worlds(scenario, clients, chunks);
        construction.push(start.elapsed());
        if sample + 1 == samples {
            worlds = built;
        } else {
            drop(built);
        }
    }
    black_box(&worlds);

    let rss_retained = linux_memory_kib("VmRSS");
    let rss_peak = linux_memory_kib("VmHWM");
    let payload_copies = worlds.iter().map(HashMap::len).sum::<usize>();
    let unique_contents = count_unique_chunks(&worlds);
    let payload_heap_bytes = worlds
        .iter()
        .flat_map(HashMap::values)
        .map(chunk_heap_bytes)
        .sum::<usize>();
    let index_lower_bound_bytes = worlds
        .iter()
        .map(|world| world.capacity() * size_of::<((i32, i32), Chunk)>())
        .sum::<usize>();
    let logical_retained_bytes = payload_heap_bytes + index_lower_bound_bytes;

    let lookup = measure_lookup(&worlds);
    let update = if matches!(scenario, Scenario::UpdateChurn) {
        measure_updates(&mut worlds)
    } else {
        Duration::ZERO
    };
    let lifecycle = if matches!(scenario, Scenario::LifecycleChurn) {
        measure_lifecycle(&mut worlds, chunks)
    } else {
        Duration::ZERO
    };

    let cleanup_start = Instant::now();
    drop(worlds);
    let cleanup = cleanup_start.elapsed();
    let rss_after_drop = linux_memory_kib("VmRSS");
    let (construction_median, construction_min, construction_max) =
        duration_summary(&mut construction);

    let extrapolated_100x441 =
        if clients == 1 && chunks == 441 && matches!(scenario, Scenario::Identical) {
            logical_retained_bytes.saturating_mul(100)
        } else {
            0
        };

    println!(
        "RESULT,model=per-client,scenario={},clients={clients},chunks_per_client={chunks},payload_copies={payload_copies},unique_contents={unique_contents},payload_heap_bytes={payload_heap_bytes},index_lower_bound_bytes={index_lower_bound_bytes},logical_retained_bytes={logical_retained_bytes},rss_baseline_kib={},rss_retained_kib={},rss_peak_kib={},rss_after_drop_kib={},construction_median_us={},construction_min_us={},construction_max_us={},lookup_total_us={},update_total_us={},lifecycle_total_us={},cleanup_us={},extrapolated_100x441_logical_bytes={extrapolated_100x441}",
        scenario.name(),
        optional_number(rss_baseline),
        optional_number(rss_retained),
        optional_number(rss_peak),
        optional_number(rss_after_drop),
        construction_median.as_micros(),
        construction_min.as_micros(),
        construction_max.as_micros(),
        lookup.as_micros(),
        update.as_micros(),
        lifecycle.as_micros(),
        cleanup.as_micros(),
    );
    ExitCode::SUCCESS
}

fn run_shared_child(
    scenario: Scenario,
    clients: usize,
    chunks: usize,
    samples: usize,
) -> ExitCode {
    drop(build_shared_worlds(scenario, clients, chunks));

    let rss_baseline = linux_memory_kib("VmRSS");
    let mut construction = Vec::with_capacity(samples);
    let mut retained = None;
    for sample in 0..samples {
        let start = Instant::now();
        let built = build_shared_worlds(scenario, clients, chunks);
        construction.push(start.elapsed());
        if sample + 1 == samples {
            retained = Some(built);
        } else {
            drop(built);
        }
    }
    let (mut worlds, store, scope) = retained.expect("positive sample count");
    black_box(&worlds);

    let rss_retained = linux_memory_kib("VmRSS");
    let rss_peak = linux_memory_kib("VmHWM");
    let payload_copies = worlds.iter().map(HashMap::len).sum::<usize>();
    let unique_payloads = unique_shared_payloads(&worlds);
    let payload_heap_bytes = unique_payloads
        .iter()
        .map(|chunk| chunk_snapshot_heap_bytes(chunk))
        .sum::<usize>();
    let index_lower_bound_bytes = worlds
        .iter()
        .map(|world| world.capacity() * size_of::<((i32, i32), Arc<ChunkSnapshot>)>())
        .sum::<usize>();
    let logical_retained_bytes = payload_heap_bytes + index_lower_bound_bytes;
    let unique_contents = unique_payloads.len();

    let lookup = measure_shared_lookup(&worlds);
    let update = if matches!(scenario, Scenario::UpdateChurn) {
        measure_shared_updates(&mut worlds, &store, &scope)
    } else {
        Duration::ZERO
    };
    let lifecycle = if matches!(scenario, Scenario::LifecycleChurn) {
        measure_shared_lifecycle(&mut worlds, chunks, &store, &scope)
    } else {
        Duration::ZERO
    };

    let cleanup_start = Instant::now();
    drop(worlds);
    drop(store);
    let cleanup = cleanup_start.elapsed();
    let rss_after_drop = linux_memory_kib("VmRSS");
    let (construction_median, construction_min, construction_max) =
        duration_summary(&mut construction);
    let extrapolated_100x441 =
        if clients == 1 && chunks == 441 && matches!(scenario, Scenario::Identical) {
            logical_retained_bytes
                .saturating_add(index_lower_bound_bytes.saturating_mul(99))
        } else {
            0
        };

    println!(
        "RESULT,model=shared,scenario={},clients={clients},chunks_per_client={chunks},payload_copies={payload_copies},unique_contents={unique_contents},payload_heap_bytes={payload_heap_bytes},index_lower_bound_bytes={index_lower_bound_bytes},logical_retained_bytes={logical_retained_bytes},rss_baseline_kib={},rss_retained_kib={},rss_peak_kib={},rss_after_drop_kib={},construction_median_us={},construction_min_us={},construction_max_us={},lookup_total_us={},update_total_us={},lifecycle_total_us={},cleanup_us={},extrapolated_100x441_logical_bytes={extrapolated_100x441}",
        scenario.name(),
        optional_number(rss_baseline),
        optional_number(rss_retained),
        optional_number(rss_peak),
        optional_number(rss_after_drop),
        construction_median.as_micros(),
        construction_min.as_micros(),
        construction_max.as_micros(),
        lookup.as_micros(),
        update.as_micros(),
        lifecycle.as_micros(),
        cleanup.as_micros(),
    );
    ExitCode::SUCCESS
}

fn benchmark_scope() -> WorldScope {
    let dimension = DimensionType {
        key: "minecraft:overworld".into(),
        min_y: -64,
        height: 384,
        logical_height: 384,
        coordinate_scale: 1.0,
        ultrawarm: false,
        has_ceiling: false,
    };
    WorldScope::new(
        ServerIdentity::new("benchmark.invalid", 25_565),
        "minecraft:benchmark",
        7,
        &dimension,
    )
}

fn build_shared_worlds(
    scenario: Scenario,
    clients: usize,
    chunks: usize,
) -> (Vec<SharedChunkMap>, Arc<SharedChunkStore>, WorldScope) {
    let store = Arc::new(SharedChunkStore::new());
    let scope = benchmark_scope();
    let base = make_dataset(chunks, 0);
    let worlds = (0..clients)
        .map(|client| {
            let dataset = match scenario {
                Scenario::Personalized => make_dataset(chunks, client as u32 + 1),
                _ => {
                    let mut dataset = base.clone();
                    if matches!(scenario, Scenario::MostlyIdentical) {
                        let changed = chunks.div_ceil(50).max(1);
                        for offset in 0..changed {
                            let index = (client * changed + offset) % chunks;
                            personalize_chunk(&mut dataset[index], client as u32 + 1);
                        }
                    }
                    dataset
                }
            };
            dataset
                .into_iter()
                .map(|chunk| {
                    let position = (chunk.x, chunk.z);
                    let snapshot = snapshot_from_chunk(chunk);
                    let snapshot = store
                        .intern(&scope, snapshot)
                        .expect("generated snapshot fingerprints");
                    (position, snapshot)
                })
                .collect()
        })
        .collect();
    (worlds, store, scope)
}

fn snapshot_from_chunk(chunk: Chunk) -> ChunkSnapshot {
    ChunkSnapshot {
        x: chunk.x,
        z: chunk.z,
        sections: chunk.sections.into_iter().map(Arc::new).collect(),
        heightmaps: Arc::new(Nbt::Compound(Vec::new())),
        block_entities: Arc::new(Vec::new()),
        light: Arc::new(ChunkLight::default()),
    }
}

fn unique_shared_payloads(worlds: &[SharedChunkMap]) -> Vec<&ChunkSnapshot> {
    let mut pointers = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for chunk in worlds.iter().flat_map(HashMap::values) {
        if pointers.insert(Arc::as_ptr(chunk) as usize) {
            unique.push(chunk.as_ref());
        }
    }
    unique
}

fn chunk_snapshot_heap_bytes(chunk: &ChunkSnapshot) -> usize {
    chunk.sections.capacity() * size_of::<Arc<ChunkSection>>()
        + chunk
            .sections
            .iter()
            .map(|section| {
                size_of::<ChunkSection>()
                    + container_heap_bytes(&section.block_states)
                    + container_heap_bytes(&section.biomes)
            })
            .sum::<usize>()
}

fn measure_shared_lookup(worlds: &[SharedChunkMap]) -> Duration {
    let positions = worlds
        .first()
        .map(|world| world.keys().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    let start = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..32 {
        for world in worlds {
            for position in &positions {
                if let Some(value) = world
                    .get(position)
                    .and_then(|chunk| chunk.sections.get(1))
                    .and_then(|section| section.block_states.get(2_047))
                {
                    checksum = checksum.wrapping_add(u64::from(value));
                }
            }
        }
    }
    black_box(checksum);
    start.elapsed()
}

fn measure_shared_updates(
    worlds: &mut [SharedChunkMap],
    store: &SharedChunkStore,
    scope: &WorldScope,
) -> Duration {
    let start = Instant::now();
    for (client, world) in worlds.iter_mut().enumerate() {
        let Some(position) = world.keys().next().copied() else {
            continue;
        };
        for round in 0..UPDATE_ROUNDS {
            let current = world.get(&position).expect("position came from map");
            let mut next = (**current).clone();
            let mut section = (*next.sections[1]).clone();
            let state = 30_000 + client as u32 * UPDATE_ROUNDS as u32 + round as u32;
            materialize_and_set(&mut section.block_states, 2_000 + round, state);
            next.sections[1] = Arc::new(section);
            let next = store
                .intern(scope, next)
                .expect("generated snapshot fingerprints");
            world.insert(position, next);
        }
    }
    start.elapsed()
}

fn measure_shared_lifecycle(
    worlds: &mut [SharedChunkMap],
    chunks: usize,
    store: &SharedChunkStore,
    scope: &WorldScope,
) -> Duration {
    let replacement = make_dataset(chunks, 0);
    let start = Instant::now();
    for round in 0..4 {
        for world in worlds.iter_mut() {
            let removed = world
                .keys()
                .copied()
                .filter(|(x, z)| (x + z + round) & 1 == 0)
                .collect::<Vec<_>>();
            for position in removed {
                world.remove(&position);
            }
            for chunk in &replacement {
                if !world.contains_key(&(chunk.x, chunk.z)) {
                    let snapshot = store
                        .intern(scope, snapshot_from_chunk(chunk.clone()))
                        .expect("generated snapshot fingerprints");
                    world.insert((chunk.x, chunk.z), snapshot);
                }
            }
        }
    }
    start.elapsed()
}

fn parse_positive(value: Option<&String>) -> Option<usize> {
    value?.parse().ok().filter(|value| *value > 0)
}

fn build_worlds(scenario: Scenario, clients: usize, chunks: usize) -> Vec<ChunkMap> {
    let base = make_dataset(chunks, 0);
    (0..clients)
        .map(|client| {
            let dataset = match scenario {
                Scenario::Personalized => make_dataset(chunks, client as u32 + 1),
                _ => {
                    let mut dataset = base.clone();
                    if matches!(scenario, Scenario::MostlyIdentical) {
                        let changed = chunks.div_ceil(50).max(1); // approximately 2%
                        for offset in 0..changed {
                            let index = (client * changed + offset) % chunks;
                            personalize_chunk(&mut dataset[index], client as u32 + 1);
                        }
                    }
                    dataset
                }
            };
            dataset
                .into_iter()
                .map(|chunk| ((chunk.x, chunk.z), chunk))
                .collect()
        })
        .collect()
}

fn make_dataset(chunks: usize, variant: u32) -> Vec<Chunk> {
    let side = (chunks as f64).sqrt().ceil() as i32;
    (0..chunks)
        .map(|index| {
            let x = index as i32 % side - side / 2;
            let z = index as i32 / side - side / 2;
            Chunk {
                x,
                z,
                sections: (0..SECTION_COUNT)
                    .map(|section| make_section(index, section, variant))
                    .collect(),
            }
        })
        .collect()
}

fn make_section(chunk: usize, section: usize, variant: u32) -> ChunkSection {
    if section % 6 == 0 {
        return ChunkSection {
            non_air_blocks: 0,
            block_states: PalettedContainer::Single(0),
            biomes: PalettedContainer::Single((chunk as u32 + variant) % 64),
        };
    }
    let palette = (0..128)
        .map(|entry| {
            1 + ((entry as u32 * 17 + chunk as u32 * 13 + section as u32 * 7 + variant * 101)
                % 24_000)
        })
        .collect();
    let block_indices = (0..BLOCKS_PER_SECTION)
        .map(|entry| ((entry + chunk * 3 + section * 11) % 128) as u32)
        .collect::<Vec<_>>();
    let biome_palette = (0..8)
        .map(|entry| (entry as u32 + chunk as u32 + variant) % 64)
        .collect();
    let biome_indices = (0..64)
        .map(|entry| ((entry + section) % 8) as u32)
        .collect::<Vec<_>>();
    ChunkSection {
        non_air_blocks: 3_072,
        block_states: PalettedContainer::Indirect {
            bits: 8,
            palette,
            data: pack_indices(8, &block_indices),
        },
        biomes: PalettedContainer::Indirect {
            bits: 3,
            palette: biome_palette,
            data: pack_indices(3, &biome_indices),
        },
    }
}

fn pack_indices(bits: u8, values: &[u32]) -> Vec<u64> {
    let values_per_long = 64 / usize::from(bits);
    let mut data = vec![0; values.len().div_ceil(values_per_long)];
    let mask = (1u64 << bits) - 1;
    for (index, value) in values.iter().copied().enumerate() {
        let word = index / values_per_long;
        let shift = (index % values_per_long) * usize::from(bits);
        data[word] |= (u64::from(value) & mask) << shift;
    }
    data
}

fn personalize_chunk(chunk: &mut Chunk, variant: u32) {
    let Some(section) = chunk
        .sections
        .iter_mut()
        .find(|section| matches!(&section.block_states, PalettedContainer::Indirect { .. }))
    else {
        return;
    };
    if let PalettedContainer::Indirect { palette, .. } = &mut section.block_states {
        palette[0] = 24_001 + variant;
    }
}

fn count_unique_chunks(worlds: &[ChunkMap]) -> usize {
    let mut buckets: HashMap<u64, Vec<&Chunk>> = HashMap::new();
    let mut unique = 0;
    for chunk in worlds.iter().flat_map(HashMap::values) {
        let fingerprint = chunk_fingerprint(chunk);
        let bucket = buckets.entry(fingerprint).or_default();
        if !bucket.contains(&chunk) {
            bucket.push(chunk);
            unique += 1;
        }
    }
    unique
}

fn chunk_fingerprint(chunk: &Chunk) -> u64 {
    let mut hash = Fnv64::default();
    hash.write(&chunk.x.to_le_bytes());
    hash.write(&chunk.z.to_le_bytes());
    for section in &chunk.sections {
        hash.write(&section.non_air_blocks.to_le_bytes());
        hash_container(&mut hash, &section.block_states);
        hash_container(&mut hash, &section.biomes);
    }
    hash.finish()
}

fn hash_container(hash: &mut Fnv64, container: &PalettedContainer) {
    match container {
        PalettedContainer::Single(value) => {
            hash.write(&[0]);
            hash.write(&value.to_le_bytes());
        }
        PalettedContainer::Indirect {
            bits,
            palette,
            data,
        } => {
            hash.write(&[1, *bits]);
            for value in palette {
                hash.write(&value.to_le_bytes());
            }
            for value in data {
                hash.write(&value.to_le_bytes());
            }
        }
        PalettedContainer::Direct { bits, data } => {
            hash.write(&[2, *bits]);
            for value in data {
                hash.write(&value.to_le_bytes());
            }
        }
    }
}

fn chunk_heap_bytes(chunk: &Chunk) -> usize {
    chunk.sections.capacity() * size_of::<ChunkSection>()
        + chunk
            .sections
            .iter()
            .map(|section| {
                container_heap_bytes(&section.block_states) + container_heap_bytes(&section.biomes)
            })
            .sum::<usize>()
}

fn container_heap_bytes(container: &PalettedContainer) -> usize {
    match container {
        PalettedContainer::Single(_) => 0,
        PalettedContainer::Indirect { palette, data, .. } => {
            palette.capacity() * size_of::<u32>() + data.capacity() * size_of::<u64>()
        }
        PalettedContainer::Direct { data, .. } => data.capacity() * size_of::<u64>(),
    }
}

fn measure_lookup(worlds: &[ChunkMap]) -> Duration {
    let positions = worlds
        .first()
        .map(|world| world.keys().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    let start = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..32 {
        for world in worlds {
            for position in &positions {
                if let Some(value) = world
                    .get(position)
                    .and_then(|chunk| chunk.sections.get(1))
                    .and_then(|section| section.block_states.get(2_047))
                {
                    checksum = checksum.wrapping_add(u64::from(value));
                }
            }
        }
    }
    black_box(checksum);
    start.elapsed()
}

fn measure_updates(worlds: &mut [ChunkMap]) -> Duration {
    let start = Instant::now();
    for (client, world) in worlds.iter_mut().enumerate() {
        let Some(position) = world.keys().next().copied() else {
            continue;
        };
        let chunk = world.get_mut(&position).expect("position came from map");
        for round in 0..UPDATE_ROUNDS {
            let state = 30_000 + client as u32 * UPDATE_ROUNDS as u32 + round as u32;
            materialize_and_set(&mut chunk.sections[1].block_states, 2_000 + round, state);
        }
    }
    start.elapsed()
}

fn measure_lifecycle(worlds: &mut [ChunkMap], chunks: usize) -> Duration {
    let replacement = make_dataset(chunks, 0);
    let start = Instant::now();
    for round in 0..4 {
        for world in worlds.iter_mut() {
            let removed = world
                .keys()
                .copied()
                .filter(|(x, z)| (x + z + round) & 1 == 0)
                .collect::<Vec<_>>();
            for position in removed {
                world.remove(&position);
            }
            for chunk in &replacement {
                world
                    .entry((chunk.x, chunk.z))
                    .or_insert_with(|| chunk.clone());
            }
        }
    }
    start.elapsed()
}

fn materialize_and_set(container: &mut PalettedContainer, index: usize, state: u32) {
    let mut values = (0..BLOCKS_PER_SECTION)
        .map(|entry| container.get(entry).expect("valid generated fixture"))
        .collect::<Vec<_>>();
    values[index] = state;
    let bits = (32 - values.iter().copied().max().unwrap_or(0).leading_zeros()).max(1) as u8;
    *container = PalettedContainer::Direct {
        bits,
        data: pack_indices(bits, &values),
    };
}

fn duration_summary(samples: &mut [Duration]) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    (
        samples[samples.len() / 2],
        samples[0],
        samples[samples.len() - 1],
    )
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

#[cfg(target_os = "linux")]
fn linux_memory_kib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(field)?.trim_start().strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn linux_memory_kib(_field: &str) -> Option<u64> {
    None
}

#[derive(Default)]
struct Fnv64(u64);

impl Fnv64 {
    fn write(&mut self, bytes: &[u8]) {
        if self.0 == 0 {
            self.0 = 0xcbf2_9ce4_8422_2325;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}
