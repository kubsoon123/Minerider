//! Configuration state: keep-alive/ping echo, known packs, finish.
//!
//! Packet ids come from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::configuration`); the
//! disconnect reason is decoded with the generated NBT layout.

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::configuration::{
    PacketCustomPayload, PacketDisconnect, PacketRegistryData, CLIENTBOUND_ADD_RESOURCE_PACK_ID,
    CLIENTBOUND_DISCONNECT_ID, CLIENTBOUND_FINISH_CONFIGURATION_ID, CLIENTBOUND_KEEP_ALIVE_ID,
    CLIENTBOUND_PING_ID, CLIENTBOUND_REGISTRY_DATA_ID, CLIENTBOUND_REMOVE_RESOURCE_PACK_ID,
    CLIENTBOUND_SELECT_KNOWN_PACKS_ID, SERVERBOUND_CUSTOM_PAYLOAD_ID,
    SERVERBOUND_FINISH_CONFIGURATION_ID, SERVERBOUND_KEEP_ALIVE_ID, SERVERBOUND_PONG_ID,
    SERVERBOUND_RESOURCE_PACK_RECEIVE_ID, SERVERBOUND_SELECT_KNOWN_PACKS_ID,
    SERVERBOUND_SETTINGS_ID,
};
use minerider_protocol::generated::v1_21_4::types::PacketCommonAddResourcePack;
use minerider_protocol::traits::{Decode, Encode};
use tracing::{debug, warn};

use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::minecraft::coverage::{clientbound_coverage, CoverageClass};
use crate::minecraft::{brand_payload, nbt_reason_text, vanilla_client_information, BRAND_CHANNEL};
use crate::network::connection::Connection;

/// Safety bound on packets read during configuration.
const MAX_CONFIGURATION_PACKETS: usize = 512;
const DIMENSION_TYPE_REGISTRY: &str = "minecraft:dimension_type";

/// Registry information retained from configuration for play-state decoding.
#[derive(Debug, Clone, Default)]
pub struct ConfigurationData {
    pub dimension_types: Vec<DimensionType>,
}

/// The dimension properties needed to decode chunks and run world physics.
#[derive(Debug, Clone, PartialEq)]
pub struct DimensionType {
    pub key: String,
    pub min_y: i32,
    pub height: i32,
    pub logical_height: i32,
    pub coordinate_scale: f64,
    pub ultrawarm: bool,
    pub has_ceiling: bool,
}

impl DimensionType {
    pub fn section_count(&self) -> usize {
        (self.height / 16) as usize
    }

    pub fn min_section_y(&self) -> i32 {
        self.min_y.div_euclid(16)
    }
}

/// Sends the two packets a vanilla client emits on entering configuration,
/// in vanilla's order: the `minecraft:brand` plugin message first, then
/// `client_information` (settings). The order is a wire-observable
/// fingerprint — the official 1.21.4 client sends brand before settings,
/// and this client previously sent them reversed.
async fn send_client_configuration(conn: &mut Connection, view_distance: i8) -> Result<()> {
    let brand = PacketCustomPayload {
        channel: BRAND_CHANNEL.to_string(),
        data: brand_payload()?,
    };
    let mut w = PacketWriter::new();
    brand.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_CUSTOM_PAYLOAD_ID, &w.freeze())
        .await?;

    let mut w = PacketWriter::new();
    vanilla_client_information(view_distance).encode(&mut w)?;
    conn.send_packet(SERVERBOUND_SETTINGS_ID, &w.freeze())
        .await?;
    debug!("sent brand and client_information");
    Ok(())
}

fn nbt_i32(value: &minerider_protocol::nbt::Nbt, key: &str) -> Result<i32> {
    match value.get(key) {
        Some(minerider_protocol::nbt::Nbt::Int(value)) => Ok(*value),
        other => Err(MineRiderError::Protocol(format!(
            "dimension_type {key} must be an int, got {other:?}"
        ))),
    }
}

fn nbt_f64(value: &minerider_protocol::nbt::Nbt, key: &str) -> Result<f64> {
    match value.get(key) {
        Some(minerider_protocol::nbt::Nbt::Double(value)) => Ok(*value),
        other => Err(MineRiderError::Protocol(format!(
            "dimension_type {key} must be a double, got {other:?}"
        ))),
    }
}

fn nbt_bool(value: &minerider_protocol::nbt::Nbt, key: &str) -> Result<bool> {
    match value.get(key) {
        Some(minerider_protocol::nbt::Nbt::Byte(value)) => Ok(*value != 0),
        other => Err(MineRiderError::Protocol(format!(
            "dimension_type {key} must be a byte, got {other:?}"
        ))),
    }
}

fn vanilla_dimension_type(key: &str) -> Option<DimensionType> {
    let (min_y, height, logical_height, coordinate_scale, ultrawarm, has_ceiling) = match key {
        "minecraft:overworld" | "minecraft:overworld_caves" => (-64, 384, 384, 1.0, false, false),
        "minecraft:the_nether" => (0, 256, 128, 8.0, true, true),
        "minecraft:the_end" => (0, 256, 256, 1.0, false, false),
        _ => return None,
    };
    Some(DimensionType {
        key: key.to_string(),
        min_y,
        height,
        logical_height,
        coordinate_scale,
        ultrawarm,
        has_ceiling,
    })
}

fn store_registry(data: &mut ConfigurationData, packet: PacketRegistryData) -> Result<()> {
    if packet.id != DIMENSION_TYPE_REGISTRY {
        return Ok(());
    }
    data.dimension_types.clear();
    data.dimension_types.reserve(packet.entries.len());
    for entry in packet.entries {
        let dimension = if let Some(value) = entry.value {
            DimensionType {
                key: entry.key,
                min_y: nbt_i32(&value, "min_y")?,
                height: nbt_i32(&value, "height")?,
                logical_height: nbt_i32(&value, "logical_height")?,
                coordinate_scale: nbt_f64(&value, "coordinate_scale")?,
                ultrawarm: nbt_bool(&value, "ultrawarm")?,
                has_ceiling: nbt_bool(&value, "has_ceiling")?,
            }
        } else {
            // The selected minecraft:core known pack omits built-in values.
            vanilla_dimension_type(&entry.key).ok_or_else(|| {
                MineRiderError::Protocol(format!(
                    "unknown dimension_type {} omitted its inline value",
                    entry.key
                ))
            })?
        };
        if dimension.height <= 0
            || dimension.height % 16 != 0
            || dimension.min_y % 16 != 0
            || !(1..=256).contains(&dimension.section_count())
        {
            return Err(MineRiderError::Protocol(format!(
                "invalid dimension_type bounds for {}: min_y {}, height {}",
                dimension.key, dimension.min_y, dimension.height
            )));
        }
        data.dimension_types.push(dimension);
    }
    Ok(())
}

/// Runs the configuration state until the server sends Finish
/// Configuration, leaving the connection in [`ConnectionState::Play`].
///
/// On entry the client sends its brand then settings, exactly as vanilla
/// does, before processing the server's configuration packets.
pub async fn run_configuration(
    conn: &mut Connection,
    view_distance: i8,
    accept_resource_packs: bool,
) -> Result<ConfigurationData> {
    send_client_configuration(conn, view_distance).await?;
    let mut data = ConfigurationData::default();

    for _ in 0..MAX_CONFIGURATION_PACKETS {
        let packet = conn.read_packet().await?;
        match packet.id {
            CLIENTBOUND_DISCONNECT_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let disconnect = PacketDisconnect::decode(&mut r)?;
                return Err(MineRiderError::Disconnected(nbt_reason_text(
                    &disconnect.reason,
                )));
            }
            CLIENTBOUND_FINISH_CONFIGURATION_ID => {
                conn.send_packet(SERVERBOUND_FINISH_CONFIGURATION_ID, &[])
                    .await?;
                conn.set_state(ConnectionState::Play);
                debug!("configuration finished");
                return Ok(data);
            }
            CLIENTBOUND_REGISTRY_DATA_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let registry = PacketRegistryData::decode(&mut r)?;
                store_registry(&mut data, registry)?;
            }
            CLIENTBOUND_KEEP_ALIVE_ID | CLIENTBOUND_PING_ID => {
                let response_id = if packet.id == CLIENTBOUND_KEEP_ALIVE_ID {
                    SERVERBOUND_KEEP_ALIVE_ID
                } else {
                    SERVERBOUND_PONG_ID
                };
                conn.send_packet(response_id, &packet.payload).await?;
            }
            CLIENTBOUND_ADD_RESOURCE_PACK_ID => {
                let mut r = PacketReader::new(&packet.payload);
                let pack = PacketCommonAddResourcePack::decode(&mut r)?;
                crate::minecraft::resource_pack::handle_offer(
                    conn,
                    SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
                    pack,
                    accept_resource_packs,
                )
                .await?;
            }
            CLIENTBOUND_REMOVE_RESOURCE_PACK_ID => {
                // No client-side pack state to remove and no wire response.
            }
            CLIENTBOUND_SELECT_KNOWN_PACKS_ID => {
                // Vanilla 1.21.4 selects the built-in core pack. Echoing every
                // offered pack can make the server omit inline registry data.
                let packs = minerider_protocol::generated::v1_21_4::types::PacketCommonSelectKnownPacks {
                    packs: vec![minerider_protocol::generated::v1_21_4::types::PacketCommonSelectKnownPacksPacksItem {
                        namespace: "minecraft".to_string(),
                        id: "core".to_string(),
                        version: "1.21.4".to_string(),
                    }],
                };
                let mut w = PacketWriter::new();
                packs.encode(&mut w)?;
                conn.send_packet(SERVERBOUND_SELECT_KNOWN_PACKS_ID, &w.freeze())
                    .await?;
            }
            other => {
                let entry = clientbound_coverage(ConnectionState::Configuration, other);
                match entry.class {
                    CoverageClass::Handled | CoverageClass::IntentionallyIgnored => debug!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        coverage = ?entry.class,
                        "ignoring configuration packet"
                    ),
                    CoverageClass::StoredForLater | CoverageClass::Unsupported => warn!(
                        id = format_args!("0x{other:02x}"),
                        len = packet.payload.len(),
                        coverage = ?entry.class,
                        status = ?entry.obligation.status,
                        "configuration packet not handled"
                    ),
                }
            }
        }
    }

    Err(MineRiderError::Protocol(
        "configuration did not finish within 512 packets".to_string(),
    ))
}
