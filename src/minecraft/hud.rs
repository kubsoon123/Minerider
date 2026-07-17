//! Headless player-facing HUD state for protocol 769.
//!
//! This module complements the existing local-player, inventory and player-list
//! stores with one snapshot-readable projection of every remaining HUD datum.
//! Generated packet structs are converted at the boundary and never leak into
//! public models.

use std::collections::BTreeMap;

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::generated::v1_21_4::play::{
    PacketAbilities, PacketDeathCombatEvent, PacketDifficulty, PacketEntityEffect,
    PacketEntityUpdateAttributes,
    PacketEntityUpdateAttributesPropertiesItemKey as WireAttributeKey, PacketGameStateChange,
    PacketHeldItemSlot, PacketInitializeWorldBorder, PacketLogin, PacketRemoveEntityEffect,
    PacketRespawn, PacketSetCooldown, PacketSetPlayerInventory, PacketSpawnPosition,
    PacketUpdateHealth, PacketUpdateTime, PacketWorldBorderCenter, PacketWorldBorderLerpSize,
    PacketWorldBorderSize, PacketWorldBorderWarningDelay, PacketWorldBorderWarningReach, SpawnInfo,
    SpawnInfoGamemode, CLIENTBOUND_ABILITIES_ID, CLIENTBOUND_DEATH_COMBAT_EVENT_ID,
    CLIENTBOUND_DIFFICULTY_ID, CLIENTBOUND_ENTITY_EFFECT_ID,
    CLIENTBOUND_ENTITY_UPDATE_ATTRIBUTES_ID, CLIENTBOUND_INITIALIZE_WORLD_BORDER_ID,
    CLIENTBOUND_REMOVE_ENTITY_EFFECT_ID, CLIENTBOUND_SET_COOLDOWN_ID,
    CLIENTBOUND_SPAWN_POSITION_ID, CLIENTBOUND_WORLD_BORDER_CENTER_ID,
    CLIENTBOUND_WORLD_BORDER_LERP_SIZE_ID, CLIENTBOUND_WORLD_BORDER_SIZE_ID,
    CLIENTBOUND_WORLD_BORDER_WARNING_DELAY_ID, CLIENTBOUND_WORLD_BORDER_WARNING_REACH_ID,
};
use minerider_protocol::generated::v1_21_4::types::{Position, Slot};
use minerider_protocol::traits::Decode;

use crate::core::error::Result;
use crate::minecraft::inventory::empty_slot;
use crate::minecraft::text::TextComponent;

pub const MAX_COOLDOWNS: usize = 1_024;
pub const MAX_EFFECTS: usize = 256;
pub const MAX_ATTRIBUTES: usize = 128;
pub const MAX_ATTRIBUTE_MODIFIERS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct HudState {
    pub entity_id: Option<i32>,
    pub vitals: Vitals,
    pub experience: Experience,
    pub game_mode: GameMode,
    pub previous_game_mode: Option<GameMode>,
    pub hardcore: bool,
    pub abilities: Abilities,
    pub selected_hotbar_slot: i32,
    pub held_item: Slot,
    pub hotbar: BTreeMap<i32, Slot>,
    pub cooldowns: BTreeMap<String, i32>,
    pub active_effects: BTreeMap<i32, StatusEffect>,
    pub attributes: BTreeMap<AttributeKey, Attribute>,
    pub death: Option<DeathInformation>,
    pub respawn: RespawnState,
    pub world_border: Option<WorldBorderState>,
    pub time: WorldTime,
    pub weather: WeatherState,
    pub difficulty: DifficultyState,
    pub spawn_position: Option<SpawnPosition>,
}

impl Default for HudState {
    fn default() -> Self {
        Self {
            entity_id: None,
            vitals: Vitals::default(),
            experience: Experience::default(),
            game_mode: GameMode::Unknown(-1),
            previous_game_mode: None,
            hardcore: false,
            abilities: Abilities::default(),
            selected_hotbar_slot: 0,
            held_item: empty_slot(),
            hotbar: BTreeMap::new(),
            cooldowns: BTreeMap::new(),
            active_effects: BTreeMap::new(),
            attributes: BTreeMap::new(),
            death: None,
            respawn: RespawnState::default(),
            world_border: None,
            time: WorldTime::default(),
            weather: WeatherState::default(),
            difficulty: DifficultyState::default(),
            spawn_position: None,
        }
    }
}

impl HudState {
    pub(crate) fn on_login(&mut self, packet: &PacketLogin) {
        self.entity_id = Some(packet.entity_id);
        self.hardcore = packet.is_hardcore;
        self.apply_spawn_info(&packet.world_state, 0);
    }

    pub(crate) fn on_respawn(&mut self, packet: &PacketRespawn) -> HudEvent {
        self.respawn.count = self.respawn.count.saturating_add(1);
        self.apply_spawn_info(&packet.world_state, packet.copy_metadata);
        HudEvent::Respawned {
            state: self.respawn.clone(),
        }
    }

    fn apply_spawn_info(&mut self, spawn: &SpawnInfo, copy_metadata: u8) {
        self.game_mode = spawn.gamemode.into();
        self.previous_game_mode = (spawn.previous_gamemode != u8::MAX)
            .then(|| GameMode::from_i32(i32::from(spawn.previous_gamemode)));
        self.respawn.game_mode = self.game_mode;
        self.respawn.previous_game_mode = self.previous_game_mode;
        self.respawn.dimension_name = spawn.name.clone();
        self.respawn.last_death_location = spawn.death.as_ref().map(|death| GlobalPosition {
            dimension_name: death.dimension_name.clone(),
            position: BlockPosition::from(death.location),
        });
        self.respawn.portal_cooldown = spawn.portal_cooldown;
        self.respawn.sea_level = spawn.sea_level;
        self.respawn.copy_metadata = copy_metadata;
    }

    pub(crate) fn on_health(&mut self, packet: &PacketUpdateHealth) -> HudEvent {
        self.vitals = Vitals {
            health: packet.health,
            food: packet.food,
            saturation: packet.food_saturation,
        };
        HudEvent::VitalsChanged {
            vitals: self.vitals,
        }
    }

    pub(crate) fn on_experience(&mut self, bar: f32, level: i32, total: i32) -> HudEvent {
        self.experience = Experience { bar, level, total };
        HudEvent::ExperienceChanged {
            experience: self.experience,
        }
    }

    pub(crate) fn on_selected_hotbar(&mut self, packet: &PacketHeldItemSlot) -> HudEvent {
        let applied = (0..=8).contains(&packet.slot);
        if applied {
            self.selected_hotbar_slot = packet.slot;
            self.held_item = self
                .hotbar
                .get(&packet.slot)
                .cloned()
                .unwrap_or_else(empty_slot);
        }
        HudEvent::HotbarChanged {
            selected_slot: self.selected_hotbar_slot,
            held_item: Box::new(self.held_item.clone()),
            applied,
        }
    }

    pub(crate) fn on_player_inventory(&mut self, packet: &PacketSetPlayerInventory) -> HudEvent {
        let applied = (0..=8).contains(&packet.slot_id);
        if applied {
            self.hotbar.insert(packet.slot_id, packet.contents.clone());
            if packet.slot_id == self.selected_hotbar_slot {
                self.held_item = packet.contents.clone();
            }
        }
        HudEvent::HotbarChanged {
            selected_slot: self.selected_hotbar_slot,
            held_item: Box::new(self.held_item.clone()),
            applied,
        }
    }

    pub(crate) fn on_time(&mut self, packet: &PacketUpdateTime) -> HudEvent {
        self.time = WorldTime {
            age: packet.age,
            raw_day_time: packet.time,
            day_time: packet.time.rem_euclid(24_000),
            ticking: packet.tick_day_time,
        };
        HudEvent::TimeChanged { time: self.time }
    }

    pub(crate) fn on_game_state(&mut self, packet: &PacketGameStateChange) -> Option<HudEvent> {
        match packet.reason {
            1 => {
                self.weather.raining = true;
                Some(HudEvent::WeatherChanged {
                    weather: self.weather,
                })
            }
            2 => {
                self.weather.raining = false;
                Some(HudEvent::WeatherChanged {
                    weather: self.weather,
                })
            }
            3 => {
                self.game_mode = GameMode::from_wire_float(packet.game_mode);
                self.respawn.game_mode = self.game_mode;
                Some(HudEvent::GameModeChanged {
                    game_mode: self.game_mode,
                })
            }
            7 => {
                self.weather.rain_level = packet.game_mode;
                Some(HudEvent::WeatherChanged {
                    weather: self.weather,
                })
            }
            8 => {
                self.weather.thunder_level = packet.game_mode;
                Some(HudEvent::WeatherChanged {
                    weather: self.weather,
                })
            }
            _ => None,
        }
    }

    pub(crate) fn apply(&mut self, update: HudUpdate) -> Option<HudEvent> {
        match update {
            HudUpdate::Abilities(abilities) => {
                self.abilities = abilities;
                Some(HudEvent::AbilitiesChanged { abilities })
            }
            HudUpdate::Cooldown { group, ticks } => {
                let applied = if ticks <= 0 {
                    self.cooldowns.remove(&group).is_some()
                } else if self.cooldowns.contains_key(&group)
                    || self.cooldowns.len() < MAX_COOLDOWNS
                {
                    self.cooldowns.insert(group.clone(), ticks);
                    true
                } else {
                    false
                };
                Some(HudEvent::CooldownChanged {
                    group: group.clone(),
                    remaining_ticks: self.cooldowns.get(&group).copied(),
                    applied,
                })
            }
            HudUpdate::SetEffect { entity_id, effect } => {
                if Some(entity_id) != self.entity_id {
                    return None;
                }
                let id = effect.id;
                let applied = if id < 0 {
                    false
                } else if self.active_effects.contains_key(&id)
                    || self.active_effects.len() < MAX_EFFECTS
                {
                    self.active_effects.insert(id, effect);
                    true
                } else {
                    false
                };
                Some(HudEvent::EffectChanged {
                    effect_id: id,
                    current: self.active_effects.get(&id).cloned(),
                    applied,
                })
            }
            HudUpdate::RemoveEffect {
                entity_id,
                effect_id,
            } => {
                if Some(entity_id) != self.entity_id {
                    return None;
                }
                let applied = self.active_effects.remove(&effect_id).is_some();
                Some(HudEvent::EffectChanged {
                    effect_id,
                    current: None,
                    applied,
                })
            }
            HudUpdate::Attributes {
                entity_id,
                attributes,
            } => {
                if Some(entity_id) != self.entity_id {
                    return None;
                }
                let mut applied = 0;
                let mut rejected = 0;
                for attribute in attributes {
                    if self.attributes.contains_key(&attribute.key)
                        || self.attributes.len() < MAX_ATTRIBUTES
                    {
                        self.attributes.insert(attribute.key, attribute);
                        applied += 1;
                    } else {
                        rejected += 1;
                    }
                }
                Some(HudEvent::AttributesChanged { applied, rejected })
            }
            HudUpdate::Death {
                player_id,
                information,
            } => {
                if Some(player_id) != self.entity_id {
                    return None;
                }
                self.death = Some((*information).clone());
                Some(HudEvent::Death { information })
            }
            HudUpdate::InitializeBorder(border) => {
                self.world_border = Some(border.clone());
                Some(HudEvent::WorldBorderChanged {
                    action: WorldBorderAction::Initialize,
                    current: Some(border),
                    applied: true,
                })
            }
            HudUpdate::BorderCenter { x, z } => {
                let applied = self.world_border.as_mut().is_some_and(|border| {
                    border.center_x = x;
                    border.center_z = z;
                    true
                });
                Some(self.border_event(WorldBorderAction::Center, applied))
            }
            HudUpdate::BorderLerp {
                old_diameter,
                new_diameter,
                speed,
            } => {
                let applied = self.world_border.as_mut().is_some_and(|border| {
                    border.old_diameter = old_diameter;
                    border.new_diameter = new_diameter;
                    border.lerp_millis = speed;
                    true
                });
                Some(self.border_event(WorldBorderAction::LerpSize, applied))
            }
            HudUpdate::BorderSize { diameter } => {
                let applied = self.world_border.as_mut().is_some_and(|border| {
                    border.old_diameter = diameter;
                    border.new_diameter = diameter;
                    border.lerp_millis = 0;
                    true
                });
                Some(self.border_event(WorldBorderAction::Size, applied))
            }
            HudUpdate::BorderWarningTime { warning_time } => {
                let applied = self.world_border.as_mut().is_some_and(|border| {
                    border.warning_time = warning_time;
                    true
                });
                Some(self.border_event(WorldBorderAction::WarningTime, applied))
            }
            HudUpdate::BorderWarningBlocks { warning_blocks } => {
                let applied = self.world_border.as_mut().is_some_and(|border| {
                    border.warning_blocks = warning_blocks;
                    true
                });
                Some(self.border_event(WorldBorderAction::WarningBlocks, applied))
            }
            HudUpdate::Difficulty(difficulty) => {
                self.difficulty = difficulty;
                Some(HudEvent::DifficultyChanged { difficulty })
            }
            HudUpdate::SpawnPosition(position) => {
                self.spawn_position = Some(position);
                Some(HudEvent::SpawnPositionChanged { position })
            }
        }
    }

    fn border_event(&self, action: WorldBorderAction, applied: bool) -> HudEvent {
        HudEvent::WorldBorderChanged {
            action,
            current: self.world_border.clone(),
            applied,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vitals {
    pub health: f32,
    pub food: i32,
    pub saturation: f32,
}

impl Default for Vitals {
    fn default() -> Self {
        Self {
            health: 20.0,
            food: 20,
            saturation: 5.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Experience {
    pub bar: f32,
    pub level: i32,
    pub total: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
    /// Raw protocol integer, or raw f32 bits for a non-integral game event.
    Unknown(i32),
}

impl GameMode {
    fn from_i32(value: i32) -> Self {
        match value {
            0 => Self::Survival,
            1 => Self::Creative,
            2 => Self::Adventure,
            3 => Self::Spectator,
            other => Self::Unknown(other),
        }
    }

    fn from_wire_float(value: f32) -> Self {
        if value.is_finite() && value.fract() == 0.0 {
            Self::from_i32(value as i32)
        } else {
            Self::Unknown(i32::from_ne_bytes(value.to_bits().to_ne_bytes()))
        }
    }
}

impl From<SpawnInfoGamemode> for GameMode {
    fn from(value: SpawnInfoGamemode) -> Self {
        match value {
            SpawnInfoGamemode::Survival => Self::Survival,
            SpawnInfoGamemode::Creative => Self::Creative,
            SpawnInfoGamemode::Adventure => Self::Adventure,
            SpawnInfoGamemode::Spectator => Self::Spectator,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Abilities {
    pub raw_flags: u8,
    pub flying_speed: f32,
    pub walking_speed: f32,
}

impl Abilities {
    pub fn invulnerable(self) -> bool {
        self.raw_flags & 0x01 != 0
    }

    pub fn flying(self) -> bool {
        self.raw_flags & 0x02 != 0
    }

    pub fn may_fly(self) -> bool {
        self.raw_flags & 0x04 != 0
    }

    pub fn instant_build(self) -> bool {
        self.raw_flags & 0x08 != 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusEffect {
    pub id: i32,
    pub amplifier: i32,
    pub duration_ticks: i32,
    pub flags: EffectFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectFlags {
    pub raw: u8,
}

impl EffectFlags {
    pub fn ambient(self) -> bool {
        self.raw & 0x01 != 0
    }

    pub fn show_particles(self) -> bool {
        self.raw & 0x02 != 0
    }

    pub fn show_icon(self) -> bool {
        self.raw & 0x04 != 0
    }

    pub fn blend(self) -> bool {
        self.raw & 0x08 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttributeKey {
    Armor,
    ArmorToughness,
    AttackDamage,
    AttackKnockback,
    AttackSpeed,
    BlockBreakSpeed,
    BlockInteractionRange,
    EntityInteractionRange,
    FallDamageMultiplier,
    FlyingSpeed,
    FollowRange,
    Gravity,
    JumpStrength,
    KnockbackResistance,
    Luck,
    MaxAbsorption,
    MaxHealth,
    MovementSpeed,
    SafeFallDistance,
    Scale,
    SpawnReinforcements,
    StepHeight,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub key: AttributeKey,
    pub base_value: f64,
    pub modifiers: Vec<AttributeModifier>,
    pub modifiers_truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeModifier {
    pub id: String,
    pub amount: f64,
    pub operation: AttributeOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeOperation {
    AddValue,
    AddMultipliedBase,
    AddMultipliedTotal,
    Unknown(i8),
}

impl From<i8> for AttributeOperation {
    fn from(value: i8) -> Self {
        match value {
            0 => Self::AddValue,
            1 => Self::AddMultipliedBase,
            2 => Self::AddMultipliedTotal,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeathInformation {
    pub player_id: i32,
    pub message: TextComponent,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RespawnState {
    pub count: u64,
    pub game_mode: GameMode,
    pub previous_game_mode: Option<GameMode>,
    pub dimension_name: String,
    pub last_death_location: Option<GlobalPosition>,
    pub portal_cooldown: i32,
    pub sea_level: i32,
    pub copy_metadata: u8,
}

impl Default for GameMode {
    fn default() -> Self {
        Self::Unknown(-1)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalPosition {
    pub dimension_name: String,
    pub position: BlockPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPosition {
    pub x: i32,
    pub y: i16,
    pub z: i32,
}

impl From<Position> for BlockPosition {
    fn from(value: Position) -> Self {
        Self {
            x: value.x,
            y: value.y,
            z: value.z,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorldBorderState {
    pub center_x: f64,
    pub center_z: f64,
    pub old_diameter: f64,
    pub new_diameter: f64,
    pub lerp_millis: i32,
    pub portal_teleport_boundary: i32,
    pub warning_blocks: i32,
    pub warning_time: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldBorderAction {
    Initialize,
    Center,
    LerpSize,
    Size,
    WarningTime,
    WarningBlocks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorldTime {
    pub age: i64,
    pub raw_day_time: i64,
    pub day_time: i64,
    pub ticking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WeatherState {
    pub raining: bool,
    pub rain_level: f32,
    pub thunder_level: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difficulty {
    Peaceful,
    Easy,
    Normal,
    Hard,
    Unknown(u8),
}

impl From<u8> for Difficulty {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Peaceful,
            1 => Self::Easy,
            2 => Self::Normal,
            3 => Self::Hard,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifficultyState {
    pub difficulty: Difficulty,
    pub locked: bool,
}

impl Default for DifficultyState {
    fn default() -> Self {
        Self {
            difficulty: Difficulty::Unknown(u8::MAX),
            locked: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpawnPosition {
    pub position: BlockPosition,
    pub angle: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HudEvent {
    VitalsChanged {
        vitals: Vitals,
    },
    ExperienceChanged {
        experience: Experience,
    },
    GameModeChanged {
        game_mode: GameMode,
    },
    AbilitiesChanged {
        abilities: Abilities,
    },
    HotbarChanged {
        selected_slot: i32,
        held_item: Box<Slot>,
        applied: bool,
    },
    CooldownChanged {
        group: String,
        remaining_ticks: Option<i32>,
        applied: bool,
    },
    EffectChanged {
        effect_id: i32,
        current: Option<StatusEffect>,
        applied: bool,
    },
    AttributesChanged {
        applied: usize,
        rejected: usize,
    },
    Death {
        information: Box<DeathInformation>,
    },
    Respawned {
        state: RespawnState,
    },
    WorldBorderChanged {
        action: WorldBorderAction,
        current: Option<WorldBorderState>,
        applied: bool,
    },
    TimeChanged {
        time: WorldTime,
    },
    WeatherChanged {
        weather: WeatherState,
    },
    DifficultyChanged {
        difficulty: DifficultyState,
    },
    SpawnPositionChanged {
        position: SpawnPosition,
    },
    PlayerListChanged {
        updated: Vec<u128>,
        removed: Vec<u128>,
        rejected: usize,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum HudUpdate {
    Abilities(Abilities),
    Cooldown {
        group: String,
        ticks: i32,
    },
    SetEffect {
        entity_id: i32,
        effect: StatusEffect,
    },
    RemoveEffect {
        entity_id: i32,
        effect_id: i32,
    },
    Attributes {
        entity_id: i32,
        attributes: Vec<Attribute>,
    },
    Death {
        player_id: i32,
        information: Box<DeathInformation>,
    },
    InitializeBorder(WorldBorderState),
    BorderCenter {
        x: f64,
        z: f64,
    },
    BorderLerp {
        old_diameter: f64,
        new_diameter: f64,
        speed: i32,
    },
    BorderSize {
        diameter: f64,
    },
    BorderWarningTime {
        warning_time: i32,
    },
    BorderWarningBlocks {
        warning_blocks: i32,
    },
    Difficulty(DifficultyState),
    SpawnPosition(SpawnPosition),
}

pub(crate) fn decode_update(id: i32, payload: &[u8]) -> Result<Option<HudUpdate>> {
    let mut input = PacketReader::new(payload);
    let update = match id {
        CLIENTBOUND_ABILITIES_ID => {
            let packet = PacketAbilities::decode(&mut input)?;
            HudUpdate::Abilities(Abilities {
                raw_flags: packet.flags as u8,
                flying_speed: packet.flying_speed,
                walking_speed: packet.walking_speed,
            })
        }
        CLIENTBOUND_SET_COOLDOWN_ID => {
            let packet = PacketSetCooldown::decode(&mut input)?;
            HudUpdate::Cooldown {
                group: packet.cooldown_group,
                ticks: packet.cooldown_ticks,
            }
        }
        CLIENTBOUND_ENTITY_EFFECT_ID => {
            let packet = PacketEntityEffect::decode(&mut input)?;
            HudUpdate::SetEffect {
                entity_id: packet.entity_id,
                effect: StatusEffect {
                    id: packet.effect_id,
                    amplifier: packet.amplifier,
                    duration_ticks: packet.duration,
                    flags: EffectFlags { raw: packet.flags },
                },
            }
        }
        CLIENTBOUND_REMOVE_ENTITY_EFFECT_ID => {
            let packet = PacketRemoveEntityEffect::decode(&mut input)?;
            HudUpdate::RemoveEffect {
                entity_id: packet.entity_id,
                effect_id: packet.effect_id,
            }
        }
        CLIENTBOUND_ENTITY_UPDATE_ATTRIBUTES_ID => {
            let packet = PacketEntityUpdateAttributes::decode(&mut input)?;
            HudUpdate::Attributes {
                entity_id: packet.entity_id,
                attributes: packet.properties.into_iter().map(attribute).collect(),
            }
        }
        CLIENTBOUND_DEATH_COMBAT_EVENT_ID => {
            let packet = PacketDeathCombatEvent::decode(&mut input)?;
            HudUpdate::Death {
                player_id: packet.player_id,
                information: Box::new(DeathInformation {
                    player_id: packet.player_id,
                    message: TextComponent::from_nbt(&packet.message),
                }),
            }
        }
        CLIENTBOUND_INITIALIZE_WORLD_BORDER_ID => {
            let packet = PacketInitializeWorldBorder::decode(&mut input)?;
            HudUpdate::InitializeBorder(WorldBorderState {
                center_x: packet.x,
                center_z: packet.z,
                old_diameter: packet.old_diameter,
                new_diameter: packet.new_diameter,
                lerp_millis: packet.speed,
                portal_teleport_boundary: packet.portal_teleport_boundary,
                warning_blocks: packet.warning_blocks,
                warning_time: packet.warning_time,
            })
        }
        CLIENTBOUND_WORLD_BORDER_CENTER_ID => {
            let packet = PacketWorldBorderCenter::decode(&mut input)?;
            HudUpdate::BorderCenter {
                x: packet.x,
                z: packet.z,
            }
        }
        CLIENTBOUND_WORLD_BORDER_LERP_SIZE_ID => {
            let packet = PacketWorldBorderLerpSize::decode(&mut input)?;
            HudUpdate::BorderLerp {
                old_diameter: packet.old_diameter,
                new_diameter: packet.new_diameter,
                speed: packet.speed,
            }
        }
        CLIENTBOUND_WORLD_BORDER_SIZE_ID => {
            let packet = PacketWorldBorderSize::decode(&mut input)?;
            HudUpdate::BorderSize {
                diameter: packet.diameter,
            }
        }
        CLIENTBOUND_WORLD_BORDER_WARNING_DELAY_ID => {
            let packet = PacketWorldBorderWarningDelay::decode(&mut input)?;
            HudUpdate::BorderWarningTime {
                warning_time: packet.warning_time,
            }
        }
        CLIENTBOUND_WORLD_BORDER_WARNING_REACH_ID => {
            let packet = PacketWorldBorderWarningReach::decode(&mut input)?;
            HudUpdate::BorderWarningBlocks {
                warning_blocks: packet.warning_blocks,
            }
        }
        CLIENTBOUND_DIFFICULTY_ID => {
            let packet = PacketDifficulty::decode(&mut input)?;
            HudUpdate::Difficulty(DifficultyState {
                difficulty: packet.difficulty.into(),
                locked: packet.difficulty_locked,
            })
        }
        CLIENTBOUND_SPAWN_POSITION_ID => {
            let packet = PacketSpawnPosition::decode(&mut input)?;
            HudUpdate::SpawnPosition(SpawnPosition {
                position: packet.location.into(),
                angle: packet.angle,
            })
        }
        _ => return Ok(None),
    };
    Ok(Some(update))
}

fn attribute(
    value: minerider_protocol::generated::v1_21_4::play::PacketEntityUpdateAttributesPropertiesItem,
) -> Attribute {
    let modifiers_truncated = value.modifiers.len() > MAX_ATTRIBUTE_MODIFIERS;
    Attribute {
        key: attribute_key(value.key),
        base_value: value.value,
        modifiers: value
            .modifiers
            .into_iter()
            .take(MAX_ATTRIBUTE_MODIFIERS)
            .map(|modifier| AttributeModifier {
                id: modifier.uuid,
                amount: modifier.amount,
                operation: modifier.operation.into(),
            })
            .collect(),
        modifiers_truncated,
    }
}

fn attribute_key(value: WireAttributeKey) -> AttributeKey {
    match value {
        WireAttributeKey::GenericArmor => AttributeKey::Armor,
        WireAttributeKey::GenericArmorToughness => AttributeKey::ArmorToughness,
        WireAttributeKey::GenericAttackDamage => AttributeKey::AttackDamage,
        WireAttributeKey::GenericAttackKnockback => AttributeKey::AttackKnockback,
        WireAttributeKey::GenericAttackSpeed => AttributeKey::AttackSpeed,
        WireAttributeKey::PlayerBlockBreakSpeed => AttributeKey::BlockBreakSpeed,
        WireAttributeKey::PlayerBlockInteractionRange => AttributeKey::BlockInteractionRange,
        WireAttributeKey::PlayerEntityInteractionRange => AttributeKey::EntityInteractionRange,
        WireAttributeKey::GenericFallDamageMultiplier => AttributeKey::FallDamageMultiplier,
        WireAttributeKey::GenericFlyingSpeed => AttributeKey::FlyingSpeed,
        WireAttributeKey::GenericFollowRange => AttributeKey::FollowRange,
        WireAttributeKey::GenericGravity => AttributeKey::Gravity,
        WireAttributeKey::GenericJumpStrength => AttributeKey::JumpStrength,
        WireAttributeKey::GenericKnockbackResistance => AttributeKey::KnockbackResistance,
        WireAttributeKey::GenericLuck => AttributeKey::Luck,
        WireAttributeKey::GenericMaxAbsorption => AttributeKey::MaxAbsorption,
        WireAttributeKey::GenericMaxHealth => AttributeKey::MaxHealth,
        WireAttributeKey::GenericMovementSpeed => AttributeKey::MovementSpeed,
        WireAttributeKey::GenericSafeFallDistance => AttributeKey::SafeFallDistance,
        WireAttributeKey::GenericScale => AttributeKey::Scale,
        WireAttributeKey::ZombieSpawnReinforcements => AttributeKey::SpawnReinforcements,
        WireAttributeKey::GenericStepHeight => AttributeKey::StepHeight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::buffer::PacketWriter;
    use minerider_protocol::generated::v1_21_4::play::{
        PacketEntityUpdateAttributesPropertiesItem,
        PacketEntityUpdateAttributesPropertiesItemModifiersItem,
    };
    use minerider_protocol::nbt::Nbt;
    use minerider_protocol::traits::Encode;

    fn apply_packet<T: Encode>(state: &mut HudState, id: i32, packet: &T) -> Option<HudEvent> {
        let mut output = PacketWriter::new();
        packet.encode(&mut output).unwrap();
        state.apply(
            decode_update(id, &output.into_inner())
                .unwrap()
                .expect("hud packet"),
        )
    }

    #[test]
    fn abilities_preserve_flags_and_speeds() {
        let mut state = HudState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_ABILITIES_ID,
            &PacketAbilities {
                flags: 0x0f,
                flying_speed: 0.05,
                walking_speed: 0.1,
            },
        );
        assert!(state.abilities.invulnerable());
        assert!(state.abilities.flying());
        assert!(state.abilities.may_fly());
        assert!(state.abilities.instant_build());
        assert_eq!(state.abilities.flying_speed, 0.05);
    }

    #[test]
    fn cooldown_and_effect_lifecycles_are_local_and_bounded() {
        let mut state = HudState {
            entity_id: Some(7),
            ..HudState::default()
        };
        apply_packet(
            &mut state,
            CLIENTBOUND_SET_COOLDOWN_ID,
            &PacketSetCooldown {
                cooldown_group: "minecraft:ender_pearl".into(),
                cooldown_ticks: 20,
            },
        );
        assert_eq!(state.cooldowns["minecraft:ender_pearl"], 20);
        apply_packet(
            &mut state,
            CLIENTBOUND_SET_COOLDOWN_ID,
            &PacketSetCooldown {
                cooldown_group: "minecraft:ender_pearl".into(),
                cooldown_ticks: 0,
            },
        );
        assert!(state.cooldowns.is_empty());

        apply_packet(
            &mut state,
            CLIENTBOUND_ENTITY_EFFECT_ID,
            &PacketEntityEffect {
                entity_id: 8,
                effect_id: 1,
                amplifier: 2,
                duration: 100,
                flags: 0x0f,
            },
        );
        assert!(state.active_effects.is_empty());
        apply_packet(
            &mut state,
            CLIENTBOUND_ENTITY_EFFECT_ID,
            &PacketEntityEffect {
                entity_id: 7,
                effect_id: 1,
                amplifier: 2,
                duration: 100,
                flags: 0x0f,
            },
        );
        let effect = &state.active_effects[&1];
        assert_eq!(effect.amplifier, 2);
        assert!(effect.flags.ambient());
        assert!(effect.flags.show_particles());
        assert!(effect.flags.show_icon());
        assert!(effect.flags.blend());
        apply_packet(
            &mut state,
            CLIENTBOUND_REMOVE_ENTITY_EFFECT_ID,
            &PacketRemoveEntityEffect {
                entity_id: 7,
                effect_id: 1,
            },
        );
        assert!(state.active_effects.is_empty());
    }

    #[test]
    fn attributes_keep_known_keys_unknown_operations_and_modifier_bound() {
        let mut state = HudState {
            entity_id: Some(7),
            ..HudState::default()
        };
        let modifiers = (0..=MAX_ATTRIBUTE_MODIFIERS)
            .map(
                |index| PacketEntityUpdateAttributesPropertiesItemModifiersItem {
                    uuid: index.to_string(),
                    amount: index as f64,
                    operation: if index == 0 { 99 } else { 0 },
                },
            )
            .collect();
        apply_packet(
            &mut state,
            CLIENTBOUND_ENTITY_UPDATE_ATTRIBUTES_ID,
            &PacketEntityUpdateAttributes {
                entity_id: 7,
                properties: vec![PacketEntityUpdateAttributesPropertiesItem {
                    key: WireAttributeKey::GenericMovementSpeed,
                    value: 0.1,
                    modifiers,
                }],
            },
        );
        let attribute = &state.attributes[&AttributeKey::MovementSpeed];
        assert_eq!(attribute.modifiers.len(), MAX_ATTRIBUTE_MODIFIERS);
        assert!(attribute.modifiers_truncated);
        assert_eq!(
            attribute.modifiers[0].operation,
            AttributeOperation::Unknown(99)
        );
    }

    #[test]
    fn death_is_structured_and_only_applies_to_local_player() {
        let mut state = HudState {
            entity_id: Some(7),
            ..HudState::default()
        };
        let message = Nbt::Compound(vec![("text".into(), Nbt::String("fell".into()))]);
        apply_packet(
            &mut state,
            CLIENTBOUND_DEATH_COMBAT_EVENT_ID,
            &PacketDeathCombatEvent {
                player_id: 8,
                message: message.clone(),
            },
        );
        assert!(state.death.is_none());
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_DEATH_COMBAT_EVENT_ID,
            &PacketDeathCombatEvent {
                player_id: 7,
                message,
            },
        );
        assert!(matches!(event, Some(HudEvent::Death { .. })));
        assert_eq!(state.death.as_ref().unwrap().message.plain_text(), "fell");
    }

    fn border() -> PacketInitializeWorldBorder {
        PacketInitializeWorldBorder {
            x: 1.0,
            z: 2.0,
            old_diameter: 100.0,
            new_diameter: 80.0,
            speed: 50,
            portal_teleport_boundary: 29_999_984,
            warning_blocks: 5,
            warning_time: 15,
        }
    }

    #[test]
    fn border_full_lifecycle_and_out_of_order_update_are_safe() {
        let mut state = HudState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_CENTER_ID,
            &PacketWorldBorderCenter { x: 9.0, z: 9.0 },
        );
        assert!(matches!(
            event,
            Some(HudEvent::WorldBorderChanged { applied: false, .. })
        ));
        apply_packet(
            &mut state,
            CLIENTBOUND_INITIALIZE_WORLD_BORDER_ID,
            &border(),
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_CENTER_ID,
            &PacketWorldBorderCenter { x: 3.0, z: 4.0 },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_LERP_SIZE_ID,
            &PacketWorldBorderLerpSize {
                old_diameter: 80.0,
                new_diameter: 40.0,
                speed: 25,
            },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_WARNING_DELAY_ID,
            &PacketWorldBorderWarningDelay { warning_time: 20 },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_WARNING_REACH_ID,
            &PacketWorldBorderWarningReach { warning_blocks: 10 },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_WORLD_BORDER_SIZE_ID,
            &PacketWorldBorderSize { diameter: 30.0 },
        );
        let border = state.world_border.unwrap();
        assert_eq!((border.center_x, border.center_z), (3.0, 4.0));
        assert_eq!(border.new_diameter, 30.0);
        assert_eq!(border.lerp_millis, 0);
        assert_eq!(border.warning_time, 20);
        assert_eq!(border.warning_blocks, 10);
    }

    #[test]
    fn difficulty_and_spawn_preserve_unknown_and_position() {
        let mut state = HudState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_DIFFICULTY_ID,
            &PacketDifficulty {
                difficulty: 99,
                difficulty_locked: true,
            },
        );
        assert_eq!(state.difficulty.difficulty, Difficulty::Unknown(99));
        assert!(state.difficulty.locked);
        apply_packet(
            &mut state,
            CLIENTBOUND_SPAWN_POSITION_ID,
            &PacketSpawnPosition {
                location: Position { x: 1, y: 64, z: 2 },
                angle: 90.0,
            },
        );
        assert_eq!(state.spawn_position.unwrap().position.y, 64);
        assert_eq!(state.spawn_position.unwrap().angle, 90.0);
    }

    #[test]
    fn hotbar_updates_only_valid_slots_and_tracks_held_item() {
        let mut state = HudState::default();
        let item = Slot {
            item_count: 0,
            value: minerider_protocol::generated::v1_21_4::types::SlotValue::V0,
        };
        let event = state.on_player_inventory(&PacketSetPlayerInventory {
            slot_id: 9,
            contents: item.clone(),
        });
        assert!(matches!(
            event,
            HudEvent::HotbarChanged { applied: false, .. }
        ));
        state.on_player_inventory(&PacketSetPlayerInventory {
            slot_id: 4,
            contents: item,
        });
        state.on_selected_hotbar(&PacketHeldItemSlot { slot: 4 });
        assert_eq!(state.selected_hotbar_slot, 4);
        assert!(state.hotbar.contains_key(&4));
    }

    #[test]
    fn existing_vitals_experience_time_weather_and_game_mode_are_typed() {
        let mut state = HudState::default();
        state.on_health(&PacketUpdateHealth {
            health: 7.0,
            food: 8,
            food_saturation: 1.5,
        });
        state.on_experience(0.5, 3, 42);
        state.on_time(&PacketUpdateTime {
            age: 100,
            time: -1,
            tick_day_time: false,
        });
        state.on_game_state(&PacketGameStateChange {
            reason: 1,
            game_mode: 0.0,
        });
        state.on_game_state(&PacketGameStateChange {
            reason: 7,
            game_mode: 0.75,
        });
        state.on_game_state(&PacketGameStateChange {
            reason: 3,
            game_mode: 2.0,
        });
        assert_eq!(state.vitals.health, 7.0);
        assert_eq!(state.experience.total, 42);
        assert_eq!(state.time.day_time, 23_999);
        assert!(!state.time.ticking);
        assert!(state.weather.raining);
        assert_eq!(state.weather.rain_level, 0.75);
        assert_eq!(state.game_mode, GameMode::Adventure);
    }

    #[test]
    fn malformed_payload_is_a_protocol_error() {
        assert!(decode_update(CLIENTBOUND_ABILITIES_ID, &[0x01]).is_err());
        assert!(decode_update(CLIENTBOUND_INITIALIZE_WORLD_BORDER_ID, &[0x00]).is_err());
    }
}
