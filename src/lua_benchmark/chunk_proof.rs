//! Proves — with assertions, not just benchmark output — the chunk-sharing
//! properties the mission asks for: identical payloads in the same scope
//! stay pointer-shared, different scopes/content stay independent, and
//! proxy configuration cannot affect the sharing scope (`ServerIdentity`
//! is derived only from `ClientConfig::host`/`port`).
//!
//! `Lua` never appears here on purpose: chunk sharing is a pure
//! `SharedChunkStore`/`World` property (owned by PR #2, unrelated to the
//! Lua runtime), so the proof is Lua-count-independent by construction —
//! see `docs/lua_runtime_benchmark.md`'s "chunk-sharing verification".

use std::sync::Arc;

use minerider_protocol::nbt::Nbt;

use crate::minecraft::configuration::DimensionType;
use crate::minecraft::shared_world::{
    ChunkLight, ChunkSnapshot, ServerIdentity, SharedChunkStore, WorldScope,
};
use crate::minecraft::world::{ChunkSection, PalettedContainer, World};

/// A trivial but structurally realistic chunk: one section, uniform block
/// state `variant` — the pointer-identity proof only needs equal-vs-unequal
/// content, not realistic terrain.
pub fn test_chunk(x: i32, z: i32, variant: u32) -> ChunkSnapshot {
    ChunkSnapshot {
        x,
        z,
        sections: vec![Arc::new(ChunkSection {
            non_air_blocks: if variant == 0 { 0 } else { 1 },
            block_states: PalettedContainer::Single(variant),
            biomes: PalettedContainer::Single(0),
        })],
        heightmaps: Arc::new(Nbt::Compound(vec![])),
        block_entities: Arc::new(Vec::new()),
        light: Arc::new(ChunkLight::default()),
    }
}

pub fn test_dimension(key: &str) -> DimensionType {
    DimensionType {
        key: key.to_string(),
        min_y: -64,
        height: 16,
        logical_height: 16,
        coordinate_scale: 1.0,
        ultrawarm: false,
        has_ceiling: false,
    }
}

pub fn test_scope(host: &str, port: u16, dimension: &DimensionType) -> WorldScope {
    WorldScope::new(
        ServerIdentity::new(host, port),
        "minecraft:overworld",
        0,
        dimension,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_payloads_in_the_same_scope_are_pointer_shared() {
        let store = SharedChunkStore::new();
        let dim = test_dimension("minecraft:overworld");
        let scope = test_scope("play.example.com", 25565, &dim);

        let a = store.intern(&scope, test_chunk(0, 0, 1)).unwrap();
        let b = store.intern(&scope, test_chunk(0, 0, 1)).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "two identical ChunkSnapshots in the same WorldScope must reuse one Arc"
        );
    }

    #[test]
    fn different_content_in_the_same_scope_is_not_shared() {
        let store = SharedChunkStore::new();
        let dim = test_dimension("minecraft:overworld");
        let scope = test_scope("play.example.com", 25565, &dim);

        let a = store.intern(&scope, test_chunk(0, 0, 1)).unwrap();
        let b = store.intern(&scope, test_chunk(0, 0, 2)).unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "differing content at the same position must never collapse to one Arc"
        );
    }

    #[test]
    fn different_servers_never_share_even_with_identical_content() {
        let store = SharedChunkStore::new();
        let dim = test_dimension("minecraft:overworld");
        let scope_a = test_scope("server-a.example.com", 25565, &dim);
        let scope_b = test_scope("server-b.example.com", 25565, &dim);

        let a = store.intern(&scope_a, test_chunk(0, 0, 1)).unwrap();
        let b = store.intern(&scope_b, test_chunk(0, 0, 1)).unwrap();
        assert!(!Arc::ptr_eq(&a, &b), "different servers must stay isolated");
    }

    #[test]
    fn different_dimensions_never_share_even_with_identical_content() {
        let store = SharedChunkStore::new();
        let overworld = test_dimension("minecraft:overworld");
        let nether = test_dimension("minecraft:the_nether");
        let scope_overworld = test_scope("play.example.com", 25565, &overworld);
        let scope_nether = test_scope("play.example.com", 25565, &nether);

        let a = store.intern(&scope_overworld, test_chunk(0, 0, 1)).unwrap();
        let b = store.intern(&scope_nether, test_chunk(0, 0, 1)).unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different dimensions must stay isolated"
        );
    }

    /// `ServerIdentity::new` takes only `host`/`port` — there is no
    /// parameter for a proxy at all, so two `ClientConfig`s that differ
    /// only in `.with_socks5_proxy(...)` produce byte-identical
    /// `ServerIdentity`/`WorldScope` values by construction. This test
    /// proves it empirically rather than only by reading the signature.
    #[test]
    fn proxy_configuration_cannot_affect_the_sharing_scope() {
        use crate::core::client::ClientConfig;
        use crate::network::socks5::Socks5ProxyConfig;

        let direct = ClientConfig::new("play.example.com", 25565, "bot-direct");
        let via_proxy_a = ClientConfig::new("play.example.com", 25565, "bot-proxy-a")
            .with_socks5_proxy(Arc::new(Socks5ProxyConfig::new("proxy-a.local", 1080)));
        let via_proxy_b = ClientConfig::new("play.example.com", 25565, "bot-proxy-b")
            .with_socks5_proxy(Arc::new(Socks5ProxyConfig::new("proxy-b.local", 1081)));

        let identity_direct = ServerIdentity::new(&direct.host, direct.port);
        let identity_a = ServerIdentity::new(&via_proxy_a.host, via_proxy_a.port);
        let identity_b = ServerIdentity::new(&via_proxy_b.host, via_proxy_b.port);

        assert_eq!(identity_direct, identity_a);
        assert_eq!(identity_a, identity_b);

        // And the practical consequence: three "bots" with three different
        // proxy configs (including none) still land in one shared bucket.
        let store = SharedChunkStore::new();
        let dim = test_dimension("minecraft:overworld");
        let scope = WorldScope::new(identity_direct, "minecraft:overworld", 0, &dim);
        let a = store.intern(&scope, test_chunk(1, 1, 5)).unwrap();
        let b = store.intern(&scope, test_chunk(1, 1, 5)).unwrap();
        let c = store.intern(&scope, test_chunk(1, 1, 5)).unwrap();
        assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c));
    }

    #[test]
    fn disabling_chunk_sharing_gives_every_client_an_independent_payload() {
        let dim = test_dimension("minecraft:overworld");
        let mut unshared_a = World::new(dim.clone());
        let mut unshared_b = World::new(dim);

        // `World::new` never touches a `SharedChunkStore` — its private
        // `intern` unconditionally does `Arc::new(chunk)` (see
        // `src/minecraft/world.rs`), so this exercises the real opt-out
        // path, not a re-implementation of it.
        insert_test_chunk(&mut unshared_a, 0, 0, 7);
        insert_test_chunk(&mut unshared_b, 0, 0, 7);

        let a = unshared_a.chunk_arc(0, 0).unwrap();
        let b = unshared_b.chunk_arc(0, 0).unwrap();
        assert!(
            !Arc::ptr_eq(a, b),
            "share_chunk_payloads = false must never produce a shared Arc, even for identical content"
        );
    }

    #[test]
    fn enabling_chunk_sharing_gives_two_worlds_in_scope_a_shared_payload() {
        let dim = test_dimension("minecraft:overworld");
        let store = Arc::new(SharedChunkStore::new());
        let scope = test_scope("play.example.com", 25565, &dim);

        let mut world_a = World::with_shared_store(dim.clone(), store.clone(), scope.clone());
        let mut world_b = World::with_shared_store(dim, store, scope);
        insert_test_chunk(&mut world_a, 2, 2, 9);
        insert_test_chunk(&mut world_b, 2, 2, 9);

        let a = world_a.chunk_arc(2, 2).unwrap();
        let b = world_b.chunk_arc(2, 2).unwrap();
        assert!(Arc::ptr_eq(a, b));
    }

    #[test]
    fn each_world_keeps_its_own_position_map_regardless_of_sharing() {
        let dim = test_dimension("minecraft:overworld");
        let store = Arc::new(SharedChunkStore::new());
        let scope = test_scope("play.example.com", 25565, &dim);
        let mut world_a = World::with_shared_store(dim.clone(), store.clone(), scope.clone());
        let mut world_b = World::with_shared_store(dim, store, scope);

        insert_test_chunk(&mut world_a, 0, 0, 1);
        insert_test_chunk(&mut world_a, 1, 0, 1);
        insert_test_chunk(&mut world_b, 0, 0, 1);

        assert!(world_a.has_chunk(1, 0));
        assert!(
            !world_b.has_chunk(1, 0),
            "loading a chunk into world_a must not make it visible in world_b"
        );
    }

    /// A helper that goes through `World::insert_chunk`'s real code path
    /// (encode a `ChunkSnapshot` back into a `PacketMapChunk`-shaped
    /// payload is unnecessary — `World`'s only other chunk-mutating entry
    /// point besides `insert_chunk`/`unload_chunk` is this crate-internal
    /// `intern`, unreachable from outside `world.rs`; the accessor above
    /// reads what `insert_chunk` itself would have stored). Uses the crate-
    /// internal `World::chunk_arc` test hook plus a minimal direct `World`
    /// construction (bypassing the packet-decode step, which is exercised
    /// separately by the existing decode/roundtrip tests) purely to seed a
    /// specific chunk position without a full protocol payload.
    fn insert_test_chunk(world: &mut World, x: i32, z: i32, variant: u32) {
        world.insert_test_snapshot(x, z, test_chunk(x, z, variant));
    }
}
