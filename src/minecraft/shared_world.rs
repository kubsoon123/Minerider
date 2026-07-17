//! Bounded interning for immutable decoded chunk snapshots.
//!
//! The store is only an ownership optimization. Every client keeps its own
//! position index and must receive a chunk before it can hold an Arc to the
//! payload. Fingerprints select a collision bucket; full equality is always
//! required before an existing payload is reused.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::play::PacketMapChunk;
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::Encode;

use crate::core::error::Result;
use crate::minecraft::configuration::DimensionType;
use crate::minecraft::world::{Chunk, ChunkSection, PalettedContainer};

const DEFAULT_SHARD_COUNT: usize = 16;
const DEFAULT_KEYS_PER_SHARD: usize = 4_096;
const DEFAULT_COLLISIONS_PER_KEY: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct BlockEntitySnapshot {
    pub local_x: u8,
    pub local_z: u8,
    pub y: i16,
    pub kind: i32,
    pub nbt: Option<Nbt>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChunkLight {
    pub sky_mask: Vec<i64>,
    pub block_mask: Vec<i64>,
    pub empty_sky_mask: Vec<i64>,
    pub empty_block_mask: Vec<i64>,
    pub sky_arrays: Vec<Vec<u8>>,
    pub block_arrays: Vec<Vec<u8>>,
}

impl ChunkLight {
    fn from_map_chunk(packet: &PacketMapChunk) -> Self {
        Self {
            sky_mask: packet.sky_light_mask.clone(),
            block_mask: packet.block_light_mask.clone(),
            empty_sky_mask: packet.empty_sky_light_mask.clone(),
            empty_block_mask: packet.empty_block_light_mask.clone(),
            sky_arrays: packet.sky_light.clone(),
            block_arrays: packet.block_light.clone(),
        }
    }
}

/// One immutable, semantically complete retained version of a chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkSnapshot {
    pub x: i32,
    pub z: i32,
    pub sections: Vec<Arc<ChunkSection>>,
    pub heightmaps: Arc<Nbt>,
    pub block_entities: Arc<Vec<BlockEntitySnapshot>>,
    pub light: Arc<ChunkLight>,
}

impl ChunkSnapshot {
    pub fn decode(packet: &PacketMapChunk, section_count: usize) -> Result<Self> {
        let decoded = Chunk::decode(packet, section_count)?;
        Ok(Self {
            x: decoded.x,
            z: decoded.z,
            sections: decoded.sections.into_iter().map(Arc::new).collect(),
            heightmaps: Arc::new(packet.heightmaps.clone()),
            block_entities: Arc::new(
                packet
                    .block_entities
                    .iter()
                    .map(|entity| BlockEntitySnapshot {
                        local_x: entity.value.x,
                        local_z: entity.value.z,
                        y: entity.y,
                        kind: entity.r#type,
                        nbt: entity.nbt_data.clone(),
                    })
                    .collect(),
            ),
            light: Arc::new(ChunkLight::from_map_chunk(packet)),
        })
    }

    pub(crate) fn fingerprint(&self) -> Result<u64> {
        let mut hash = Fnv64::new();
        hash.write(&self.x.to_le_bytes());
        hash.write(&self.z.to_le_bytes());
        write_len(&mut hash, self.sections.len());
        for section in &self.sections {
            hash.write(&section.non_air_blocks.to_le_bytes());
            hash_container(&mut hash, &section.block_states);
            hash_container(&mut hash, &section.biomes);
        }
        hash_nbt(&mut hash, &self.heightmaps)?;
        write_len(&mut hash, self.block_entities.len());
        for entity in self.block_entities.iter() {
            hash.write(&[entity.local_x, entity.local_z]);
            hash.write(&entity.y.to_le_bytes());
            hash.write(&entity.kind.to_le_bytes());
            match &entity.nbt {
                Some(nbt) => {
                    hash.write(&[1]);
                    hash_nbt(&mut hash, nbt)?;
                }
                None => hash.write(&[0]),
            }
        }
        hash_light(&mut hash, &self.light);
        Ok(hash.finish())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerIdentity {
    host: Arc<str>,
    port: u16,
}

impl ServerIdentity {
    pub fn new(host: impl Into<Arc<str>>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DimensionIdentity {
    key: Arc<str>,
    min_y: i32,
    height: i32,
    logical_height: i32,
    coordinate_scale_bits: u64,
    ultrawarm: bool,
    has_ceiling: bool,
}

impl From<&DimensionType> for DimensionIdentity {
    fn from(value: &DimensionType) -> Self {
        Self {
            key: Arc::from(value.key.as_str()),
            min_y: value.min_y,
            height: value.height,
            logical_height: value.logical_height,
            coordinate_scale_bits: value.coordinate_scale.to_bits(),
            ultrawarm: value.ultrawarm,
            has_ceiling: value.has_ceiling,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorldScope {
    server: ServerIdentity,
    generation: u64,
    dimension: DimensionIdentity,
}

impl WorldScope {
    pub fn new(server: ServerIdentity, generation: u64, dimension: &DimensionType) -> Self {
        Self {
            server,
            generation,
            dimension: dimension.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InternKey {
    scope: WorldScope,
    x: i32,
    z: i32,
    fingerprint: u64,
}

#[derive(Debug, Default)]
struct StoreShard {
    entries: HashMap<InternKey, Vec<Weak<ChunkSnapshot>>>,
    insertion_order: VecDeque<InternKey>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedChunkStoreStats {
    pub cache_keys: usize,
    pub weak_entries: usize,
    pub live_payloads: usize,
    pub evictions: u64,
    pub uncached_collision_payloads: u64,
}

/// Sharded, bounded weak interning table. It never owns a chunk strongly.
#[derive(Debug)]
pub struct SharedChunkStore {
    shards: Box<[Mutex<StoreShard>]>,
    max_keys_per_shard: usize,
    max_collisions_per_key: usize,
    evictions: AtomicU64,
    uncached_collision_payloads: AtomicU64,
}

impl Default for SharedChunkStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedChunkStore {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_SHARD_COUNT,
            DEFAULT_KEYS_PER_SHARD,
            DEFAULT_COLLISIONS_PER_KEY,
        )
    }

    fn with_limits(shards: usize, keys_per_shard: usize, collisions_per_key: usize) -> Self {
        assert!(shards > 0);
        assert!(keys_per_shard > 0);
        assert!(collisions_per_key > 0);
        Self {
            shards: (0..shards)
                .map(|_| Mutex::new(StoreShard::default()))
                .collect(),
            max_keys_per_shard: keys_per_shard,
            max_collisions_per_key: collisions_per_key,
            evictions: AtomicU64::new(0),
            uncached_collision_payloads: AtomicU64::new(0),
        }
    }

    pub fn intern(
        &self,
        scope: &WorldScope,
        snapshot: ChunkSnapshot,
    ) -> Result<Arc<ChunkSnapshot>> {
        let fingerprint = snapshot.fingerprint()?;
        Ok(self.intern_with_fingerprint(scope, snapshot, fingerprint))
    }

    fn intern_with_fingerprint(
        &self,
        scope: &WorldScope,
        snapshot: ChunkSnapshot,
        fingerprint: u64,
    ) -> Arc<ChunkSnapshot> {
        let key = InternKey {
            scope: scope.clone(),
            x: snapshot.x,
            z: snapshot.z,
            fingerprint,
        };
        let shard_index = self.shard_index(&key);
        let mut shard = lock_unpoisoned(&self.shards[shard_index]);

        if let Some(bucket) = shard.entries.get_mut(&key) {
            bucket.retain(|entry| entry.strong_count() > 0);
            for candidate in bucket.iter().filter_map(Weak::upgrade) {
                if *candidate == snapshot {
                    return candidate;
                }
            }
            let snapshot = Arc::new(snapshot);
            if bucket.len() < self.max_collisions_per_key {
                bucket.push(Arc::downgrade(&snapshot));
            } else {
                self.uncached_collision_payloads
                    .fetch_add(1, Ordering::Relaxed);
            }
            return snapshot;
        }

        while shard.entries.len() >= self.max_keys_per_shard {
            let Some(oldest) = shard.insertion_order.pop_front() else {
                break;
            };
            if shard.entries.remove(&oldest).is_some() {
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        let snapshot = Arc::new(snapshot);
        shard
            .entries
            .insert(key.clone(), vec![Arc::downgrade(&snapshot)]);
        shard.insertion_order.push_back(key);
        snapshot
    }

    pub fn prune(&self) {
        for shard in &self.shards {
            let mut shard = lock_unpoisoned(shard);
            shard.entries.retain(|_, bucket| {
                bucket.retain(|entry| entry.strong_count() > 0);
                !bucket.is_empty()
            });
            let live_keys = shard.entries.keys().cloned().collect::<HashSet<_>>();
            shard.insertion_order.retain(|key| live_keys.contains(key));
        }
    }

    pub fn stats(&self) -> SharedChunkStoreStats {
        let mut cache_keys = 0;
        let mut weak_entries = 0;
        let mut live = HashSet::new();
        for shard in &self.shards {
            let shard = lock_unpoisoned(shard);
            cache_keys += shard.entries.len();
            for bucket in shard.entries.values() {
                weak_entries += bucket.len();
                for payload in bucket.iter().filter_map(Weak::upgrade) {
                    live.insert(Arc::as_ptr(&payload) as usize);
                }
            }
        }
        SharedChunkStoreStats {
            cache_keys,
            weak_entries,
            live_payloads: live.len(),
            evictions: self.evictions.load(Ordering::Relaxed),
            uncached_collision_payloads: self.uncached_collision_payloads.load(Ordering::Relaxed),
        }
    }

    fn shard_index(&self, key: &InternKey) -> usize {
        let mixed = key.fingerprint
            ^ (key.x as u32 as u64).rotate_left(17)
            ^ (key.z as u32 as u64).rotate_left(37)
            ^ key.scope.generation.rotate_left(7)
            ^ u64::from(key.scope.server.port);
        mixed as usize % self.shards.len()
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            write_len(hash, palette.len());
            for value in palette {
                hash.write(&value.to_le_bytes());
            }
            write_len(hash, data.len());
            for value in data {
                hash.write(&value.to_le_bytes());
            }
        }
        PalettedContainer::Direct { bits, data } => {
            hash.write(&[2, *bits]);
            write_len(hash, data.len());
            for value in data {
                hash.write(&value.to_le_bytes());
            }
        }
    }
}

fn hash_nbt(hash: &mut Fnv64, nbt: &Nbt) -> Result<()> {
    let mut encoded = PacketWriter::new();
    nbt.encode(&mut encoded)?;
    let encoded = encoded.into_inner();
    write_len(hash, encoded.len());
    hash.write(&encoded);
    Ok(())
}

fn hash_light(hash: &mut Fnv64, light: &ChunkLight) {
    hash_i64s(hash, &light.sky_mask);
    hash_i64s(hash, &light.block_mask);
    hash_i64s(hash, &light.empty_sky_mask);
    hash_i64s(hash, &light.empty_block_mask);
    hash_byte_arrays(hash, &light.sky_arrays);
    hash_byte_arrays(hash, &light.block_arrays);
}

fn hash_i64s(hash: &mut Fnv64, values: &[i64]) {
    write_len(hash, values.len());
    for value in values {
        hash.write(&value.to_le_bytes());
    }
}

fn hash_byte_arrays(hash: &mut Fnv64, arrays: &[Vec<u8>]) {
    write_len(hash, arrays.len());
    for array in arrays {
        write_len(hash, array.len());
        hash.write(array);
    }
}

fn write_len(hash: &mut Fnv64, value: usize) {
    hash.write(&(value as u64).to_le_bytes());
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn dimension(key: &str) -> DimensionType {
        DimensionType {
            key: key.into(),
            min_y: -64,
            height: 384,
            logical_height: 384,
            coordinate_scale: 1.0,
            ultrawarm: false,
            has_ceiling: false,
        }
    }

    fn scope(server: &str, generation: u64, dimension_key: &str) -> WorldScope {
        WorldScope::new(
            ServerIdentity::new(server, 25_565),
            generation,
            &dimension(dimension_key),
        )
    }

    fn snapshot(state: u32) -> ChunkSnapshot {
        ChunkSnapshot {
            x: 2,
            z: -3,
            sections: vec![Arc::new(ChunkSection {
                non_air_blocks: u16::from(state != 0),
                block_states: PalettedContainer::Single(state),
                biomes: PalettedContainer::Single(1),
            })],
            heightmaps: Arc::new(Nbt::Compound(Vec::new())),
            block_entities: Arc::new(Vec::new()),
            light: Arc::new(ChunkLight::default()),
        }
    }

    #[test]
    fn identical_snapshot_is_canonical_within_scope() {
        let store = SharedChunkStore::new();
        let scope = scope("server-a", 0, "minecraft:overworld");
        let first = store.intern(&scope, snapshot(1)).unwrap();
        let second = store.intern(&scope, snapshot(1)).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(store.stats().live_payloads, 1);
    }

    #[test]
    fn content_and_scope_boundaries_never_alias() {
        let store = SharedChunkStore::new();
        let base = scope("server-a", 0, "minecraft:overworld");
        let first = store.intern(&base, snapshot(1)).unwrap();
        let different_content = store.intern(&base, snapshot(2)).unwrap();
        let different_server = store
            .intern(&scope("server-b", 0, "minecraft:overworld"), snapshot(1))
            .unwrap();
        let different_dimension = store
            .intern(&scope("server-a", 0, "minecraft:the_nether"), snapshot(1))
            .unwrap();
        let different_generation = store
            .intern(&scope("server-a", 1, "minecraft:overworld"), snapshot(1))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &different_content));
        assert!(!Arc::ptr_eq(&first, &different_server));
        assert!(!Arc::ptr_eq(&first, &different_dimension));
        assert!(!Arc::ptr_eq(&first, &different_generation));
    }

    #[test]
    fn forced_fingerprint_collision_still_checks_full_equality() {
        let store = SharedChunkStore::with_limits(1, 8, 8);
        let scope = scope("server-a", 0, "minecraft:overworld");
        let first = store.intern_with_fingerprint(&scope, snapshot(1), 7);
        let second = store.intern_with_fingerprint(&scope, snapshot(2), 7);
        let first_again = store.intern_with_fingerprint(&scope, snapshot(1), 7);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &first_again));
    }

    #[test]
    fn weak_entries_do_not_keep_payloads_alive() {
        let store = SharedChunkStore::new();
        let scope = scope("server-a", 0, "minecraft:overworld");
        let payload = store.intern(&scope, snapshot(1)).unwrap();
        assert_eq!(Arc::strong_count(&payload), 1);
        drop(payload);
        store.prune();
        assert_eq!(store.stats().live_payloads, 0);
        assert_eq!(store.stats().cache_keys, 0);
    }

    #[test]
    fn key_and_collision_bounds_are_enforced() {
        let store = SharedChunkStore::with_limits(1, 2, 2);
        let scope = scope("server-a", 0, "minecraft:overworld");
        let mut live = Vec::new();
        for state in 0..8 {
            let mut value = snapshot(state);
            value.x = state as i32;
            live.push(store.intern(&scope, value).unwrap());
        }
        assert_eq!(live.len(), 8);
        assert!(store.stats().cache_keys <= 2);
        assert!(store.stats().evictions >= 6);

        let first = store.intern_with_fingerprint(&scope, snapshot(100), 9);
        let second = store.intern_with_fingerprint(&scope, snapshot(101), 9);
        let third = store.intern_with_fingerprint(&scope, snapshot(102), 9);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&second, &third));
        assert_eq!(store.stats().uncached_collision_payloads, 1);
    }

    #[test]
    fn concurrent_publication_returns_one_canonical_payload() {
        let store = Arc::new(SharedChunkStore::new());
        let scope = scope("server-a", 0, "minecraft:overworld");
        let handles = (0..16)
            .map(|_| {
                let store = store.clone();
                let scope = scope.clone();
                thread::spawn(move || store.intern(&scope, snapshot(1)).unwrap())
            })
            .collect::<Vec<_>>();
        let payloads = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        for payload in &payloads[1..] {
            assert!(Arc::ptr_eq(&payloads[0], payload));
        }
    }
}
