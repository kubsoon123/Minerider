//! Dimension-aware chunk storage and 1.21.4 paletted-container decoding.

use std::collections::HashMap;

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::generated::v1_21_4::play::{
    PacketBlockChange, PacketMapChunk, PacketMultiBlockChange, PacketUnloadChunk,
};

use crate::core::error::{MineRiderError, Result};
use crate::minecraft::configuration::DimensionType;
use crate::minecraft::physics::{collision_boxes, Aabb};

const BLOCKS_PER_SECTION: usize = 16 * 16 * 16;
const BIOMES_PER_SECTION: usize = 4 * 4 * 4;
const MAX_PALETTE_BITS: u8 = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum PalettedContainer {
    Single(u32),
    Indirect {
        bits: u8,
        palette: Vec<u32>,
        data: Vec<u64>,
    },
    Direct {
        bits: u8,
        data: Vec<u64>,
    },
}

impl PalettedContainer {
    fn decode(
        input: &mut PacketReader<'_>,
        entries: usize,
        min_indirect_bits: u8,
        max_indirect_bits: u8,
    ) -> Result<Self> {
        let wire_bits = input.get_u8()?;
        if wire_bits == 0 {
            let value = nonnegative_varint(input, "single palette value")?;
            let long_count = nonnegative_varint(input, "single palette data length")?;
            if long_count != 0 {
                return Err(protocol(format!(
                    "single-valued palette has {long_count} packed longs"
                )));
            }
            return Ok(Self::Single(value));
        }

        if wire_bits > MAX_PALETTE_BITS {
            return Err(protocol(format!(
                "paletted container uses unsupported {wire_bits} bits"
            )));
        }
        let indirect = wire_bits <= max_indirect_bits;
        let bits = if indirect {
            wire_bits.max(min_indirect_bits)
        } else {
            wire_bits
        };
        let palette = if indirect {
            let count = nonnegative_varint(input, "palette length")? as usize;
            let capacity = 1usize
                .checked_shl(bits.into())
                .ok_or_else(|| protocol("palette bit width overflow"))?;
            if count == 0 || count > capacity {
                return Err(protocol(format!(
                    "palette length {count} outside 1..={capacity} for {bits} bits"
                )));
            }
            let mut palette = Vec::with_capacity(count);
            for _ in 0..count {
                palette.push(nonnegative_varint(input, "palette value")?);
            }
            Some(palette)
        } else {
            None
        };

        let values_per_long = 64 / usize::from(bits);
        if values_per_long == 0 {
            return Err(protocol("paletted container has zero values per long"));
        }
        let expected_longs = entries.div_ceil(values_per_long);
        let long_count = nonnegative_varint(input, "palette data length")? as usize;
        if long_count != expected_longs {
            return Err(protocol(format!(
                "paletted container has {long_count} longs, expected {expected_longs}"
            )));
        }
        let mut data = Vec::with_capacity(long_count);
        for _ in 0..long_count {
            data.push(input.get_u64()?);
        }

        Ok(match palette {
            Some(palette) => Self::Indirect {
                bits,
                palette,
                data,
            },
            None => Self::Direct { bits, data },
        })
    }

    pub fn get(&self, index: usize) -> Option<u32> {
        match self {
            Self::Single(value) => Some(*value),
            Self::Indirect {
                bits,
                palette,
                data,
            } => packed_value(*bits, data, index).and_then(|i| palette.get(i as usize).copied()),
            Self::Direct { bits, data } => packed_value(*bits, data, index),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkSection {
    pub non_air_blocks: u16,
    pub block_states: PalettedContainer,
    pub biomes: PalettedContainer,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub x: i32,
    pub z: i32,
    pub sections: Vec<ChunkSection>,
}

impl Chunk {
    pub fn decode(packet: &PacketMapChunk, section_count: usize) -> Result<Self> {
        let mut input = PacketReader::new(&packet.chunk_data);
        let mut sections = Vec::with_capacity(section_count);
        for _ in 0..section_count {
            let non_air_blocks = input.get_i16()?;
            if non_air_blocks < 0 || non_air_blocks as usize > BLOCKS_PER_SECTION {
                return Err(protocol(format!(
                    "invalid non-air block count {non_air_blocks}"
                )));
            }
            sections.push(ChunkSection {
                non_air_blocks: non_air_blocks as u16,
                block_states: PalettedContainer::decode(&mut input, BLOCKS_PER_SECTION, 4, 8)?,
                biomes: PalettedContainer::decode(&mut input, BIOMES_PER_SECTION, 1, 3)?,
            });
        }
        if !input.is_empty() {
            return Err(protocol(format!(
                "{} trailing bytes after {section_count} chunk sections",
                input.remaining()
            )));
        }
        Ok(Self {
            x: packet.x,
            z: packet.z,
            sections,
        })
    }
}

#[derive(Debug, Clone)]
pub struct World {
    pub dimension: DimensionType,
    chunks: HashMap<(i32, i32), Chunk>,
}

impl World {
    pub fn new(dimension: DimensionType) -> Self {
        Self {
            dimension,
            chunks: HashMap::new(),
        }
    }

    pub fn insert_chunk(&mut self, packet: &PacketMapChunk) -> Result<()> {
        let chunk = Chunk::decode(packet, self.dimension.section_count())?;
        self.chunks.insert((chunk.x, chunk.z), chunk);
        Ok(())
    }

    pub fn unload_chunk(&mut self, packet: &PacketUnloadChunk) {
        self.chunks.remove(&(packet.chunk_x, packet.chunk_z));
    }

    pub fn has_chunk(&self, x: i32, z: i32) -> bool {
        self.chunks.contains_key(&(x, z))
    }

    pub fn block_state(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        if y < self.dimension.min_y || y >= self.dimension.min_y + self.dimension.height {
            return None;
        }
        let chunk = self.chunks.get(&(x.div_euclid(16), z.div_euclid(16)))?;
        let section_index = (y - self.dimension.min_y).div_euclid(16) as usize;
        let section = chunk.sections.get(section_index)?;
        let local_x = x.rem_euclid(16) as usize;
        let local_y = y.rem_euclid(16) as usize;
        let local_z = z.rem_euclid(16) as usize;
        section
            .block_states
            .get((local_y * 16 + local_z) * 16 + local_x)
    }

    pub fn collision_boxes(&self, area: Aabb, out: &mut Vec<Aabb>) -> Result<()> {
        let min_x = area.min_x.floor() as i32;
        let max_x = area.max_x.ceil() as i32;
        let min_y = area.min_y.floor() as i32;
        let max_y = area.max_y.ceil() as i32;
        let min_z = area.min_z.floor() as i32;
        let max_z = area.max_z.ceil() as i32;
        for y in min_y..max_y {
            for z in min_z..max_z {
                for x in min_x..max_x {
                    let state = self.block_state(x, y, z).ok_or_else(|| {
                        protocol(format!(
                            "collision query crosses unloaded block {x},{y},{z}"
                        ))
                    })?;
                    let shapes = collision_boxes(state)
                        .ok_or_else(|| protocol(format!("unknown block state id {state}")))?;
                    out.extend(
                        shapes
                            .iter()
                            .map(|shape| shape.moved(x as f64, y as f64, z as f64)),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn apply_block_change(&mut self, packet: &PacketBlockChange) -> Result<()> {
        self.set_block_state(
            packet.location.x,
            i32::from(packet.location.y),
            packet.location.z,
            packet.r#type as u32,
        )
    }

    pub fn apply_multi_block_change(&mut self, packet: &PacketMultiBlockChange) -> Result<()> {
        for &record in &packet.records {
            if record < 0 {
                return Err(protocol(format!("negative multi-block record {record}")));
            }
            let record = record as u32;
            let state = record >> 12;
            let local = record & 0xfff;
            let x = packet.chunk_coordinates.x * 16 + ((local >> 8) & 0xf) as i32;
            let z = packet.chunk_coordinates.z * 16 + ((local >> 4) & 0xf) as i32;
            let y = packet.chunk_coordinates.y * 16 + (local & 0xf) as i32;
            self.set_block_state(x, y, z, state)?;
        }
        Ok(())
    }

    fn set_block_state(&mut self, x: i32, y: i32, z: i32, state: u32) -> Result<()> {
        let chunk = self
            .chunks
            .get_mut(&(x.div_euclid(16), z.div_euclid(16)))
            .ok_or_else(|| protocol(format!("block update for unloaded chunk at {x},{z}")))?;
        let section_index = (y - self.dimension.min_y).div_euclid(16);
        if section_index < 0 || section_index as usize >= chunk.sections.len() {
            return Err(protocol(format!("block update y {y} outside dimension")));
        }
        let section = &mut chunk.sections[section_index as usize];
        let index = (y.rem_euclid(16) as usize * 16 + z.rem_euclid(16) as usize) * 16
            + x.rem_euclid(16) as usize;
        materialize_and_set(&mut section.block_states, BLOCKS_PER_SECTION, index, state)
    }
}

fn materialize_and_set(
    container: &mut PalettedContainer,
    entries: usize,
    index: usize,
    state: u32,
) -> Result<()> {
    if index >= entries {
        return Err(protocol(format!("palette index {index} outside {entries}")));
    }
    let mut values = (0..entries)
        .map(|i| {
            container
                .get(i)
                .ok_or_else(|| protocol(format!("invalid palette value at index {i}")))
        })
        .collect::<Result<Vec<_>>>()?;
    values[index] = state;
    let bits = 32 - values.iter().copied().max().unwrap_or(0).leading_zeros();
    let bits = bits.max(1) as u8;
    let values_per_long = 64 / usize::from(bits);
    let mut data = vec![0u64; entries.div_ceil(values_per_long)];
    let mask = (1u64 << bits) - 1;
    for (i, value) in values.into_iter().enumerate() {
        let long = i / values_per_long;
        let shift = (i % values_per_long) * usize::from(bits);
        data[long] |= (u64::from(value) & mask) << shift;
    }
    *container = PalettedContainer::Direct { bits, data };
    Ok(())
}

fn packed_value(bits: u8, data: &[u64], index: usize) -> Option<u32> {
    let values_per_long = 64 / usize::from(bits);
    let long = *data.get(index / values_per_long)?;
    let shift = (index % values_per_long) * usize::from(bits);
    let mask = if bits == 32 {
        u32::MAX as u64
    } else {
        (1u64 << bits) - 1
    };
    Some(((long >> shift) & mask) as u32)
}

fn nonnegative_varint(input: &mut PacketReader<'_>, what: &str) -> Result<u32> {
    let value = input.get_varint()?;
    u32::try_from(value).map_err(|_| protocol(format!("negative {what}: {value}")))
}

fn protocol(message: impl Into<String>) -> MineRiderError {
    MineRiderError::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    use minerider_protocol::buffer::PacketWriter;
    use minerider_protocol::generated::v1_21_4::play::PacketMapChunk;
    use minerider_protocol::nbt::Nbt;

    use super::*;

    fn dimension() -> DimensionType {
        DimensionType {
            key: "minecraft:overworld".into(),
            min_y: -64,
            height: 384,
            logical_height: 384,
            coordinate_scale: 1.0,
            ultrawarm: false,
            has_ceiling: false,
        }
    }

    fn single_section(block: u32, biome: u32) -> Vec<u8> {
        let mut out = PacketWriter::new();
        out.put_i16(if block == 0 { 0 } else { 4096 });
        out.put_u8(0);
        out.put_varint(block as i32);
        out.put_varint(0);
        out.put_u8(0);
        out.put_varint(biome as i32);
        out.put_varint(0);
        out.into_inner().to_vec()
    }

    fn packet(data: Vec<u8>) -> PacketMapChunk {
        PacketMapChunk {
            x: -1,
            z: 2,
            heightmaps: Nbt::Compound(vec![]),
            chunk_data: data,
            block_entities: vec![],
            sky_light_mask: vec![],
            block_light_mask: vec![],
            empty_sky_light_mask: vec![],
            empty_block_light_mask: vec![],
            sky_light: vec![],
            block_light: vec![],
        }
    }

    #[test]
    fn decodes_dimension_number_of_single_value_sections() {
        let mut data = Vec::new();
        for section in 0..24 {
            data.extend(single_section(section, 1));
        }
        let chunk = Chunk::decode(&packet(data), 24).unwrap();
        assert_eq!(chunk.sections.len(), 24);
        assert_eq!(chunk.sections[0].block_states.get(4095), Some(0));
        assert_eq!(chunk.sections[23].block_states.get(0), Some(23));
    }

    #[test]
    fn rejects_trailing_section_data() {
        let mut data = single_section(0, 0);
        data.push(1);
        assert!(Chunk::decode(&packet(data), 1).is_err());
    }

    #[test]
    fn negative_coordinates_use_euclidean_chunk_math() {
        let mut data = Vec::new();
        for _ in 0..24 {
            data.extend(single_section(5, 0));
        }
        let mut world = World::new(dimension());
        world.insert_chunk(&packet(data)).unwrap();
        assert_eq!(world.block_state(-1, -64, 32), Some(5));
        assert_eq!(world.block_state(-16, -64, 47), Some(5));
    }

    #[test]
    fn block_change_materializes_single_palette() {
        let mut data = Vec::new();
        for _ in 0..24 {
            data.extend(single_section(0, 0));
        }
        let mut world = World::new(dimension());
        world.insert_chunk(&packet(data)).unwrap();
        world.set_block_state(-1, 64, 32, 42).unwrap();
        assert_eq!(world.block_state(-1, 64, 32), Some(42));
        assert_eq!(world.block_state(-2, 64, 32), Some(0));
    }
}
