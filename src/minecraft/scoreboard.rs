//! Headless protocol-769 scoreboard and team state.
//!
//! The generated packet switch enums stop at this module boundary. Public
//! models use stable identities, deterministic ordering, the shared
//! [`TextComponent`] representation, and explicit `Unknown` variants for
//! forward compatibility. Every server-controlled collection is bounded.

use std::collections::{BTreeMap, BTreeSet};

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::generated::v1_21_4::play::{
    PacketResetScore, PacketScoreboardDisplayObjective, PacketScoreboardObjective,
    PacketScoreboardObjectiveDisplayText, PacketScoreboardObjectiveNumberFormat,
    PacketScoreboardObjectiveStyling, PacketScoreboardObjectiveStylingV0,
    PacketScoreboardObjectiveStylingV2, PacketScoreboardObjectiveType, PacketScoreboardScore,
    PacketScoreboardScoreStyling, PacketTeams, PacketTeamsCollisionRule, PacketTeamsFormatting,
    PacketTeamsFriendlyFire, PacketTeamsName, PacketTeamsNameTagVisibility, PacketTeamsPlayers,
    PacketTeamsPrefix, PacketTeamsSuffix, CLIENTBOUND_RESET_SCORE_ID,
    CLIENTBOUND_SCOREBOARD_DISPLAY_OBJECTIVE_ID, CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
    CLIENTBOUND_SCOREBOARD_SCORE_ID, CLIENTBOUND_TEAMS_ID,
};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::Decode;

use crate::core::error::Result;
use crate::minecraft::text::TextComponent;

pub const MAX_OBJECTIVES: usize = 256;
pub const MAX_DISPLAY_SLOTS: usize = 64;
pub const MAX_SCORES: usize = 16_384;
pub const MAX_TEAMS: usize = 1_024;
pub const MAX_TEAM_MEMBERS: usize = 16_384;

/// Complete scoreboard state included in every play snapshot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScoreboardState {
    pub objectives: BTreeMap<String, Objective>,
    pub display_slots: BTreeMap<DisplaySlot, String>,
    pub scores: BTreeMap<ScoreKey, Score>,
    pub teams: BTreeMap<String, Team>,
    member_teams: BTreeMap<String, String>,
}

impl ScoreboardState {
    /// Returns the one team currently owning this scoreboard entry.
    pub fn team_for_member(&self, member: &str) -> Option<&str> {
        self.member_teams.get(member).map(String::as_str)
    }

    pub(crate) fn apply(&mut self, update: ScoreboardUpdate) -> ScoreboardEvent {
        match update {
            ScoreboardUpdate::Objective {
                name,
                action,
                definition,
            } => self.apply_objective(name, action, definition),
            ScoreboardUpdate::DisplaySlot { slot, objective } => {
                self.apply_display_slot(slot, objective)
            }
            ScoreboardUpdate::SetScore(score) => self.apply_score(*score),
            ScoreboardUpdate::ResetScore { owner, objective } => {
                self.apply_score_reset(owner, objective)
            }
            ScoreboardUpdate::Team(update) => self.apply_team(*update),
        }
    }

    fn apply_objective(
        &mut self,
        name: String,
        action: ObjectiveAction,
        definition: Option<Box<ObjectiveDefinition>>,
    ) -> ScoreboardEvent {
        let mut detached_slots = 0;
        let mut removed_scores = 0;
        let applied = match action {
            ObjectiveAction::Create => definition.is_some_and(|definition| {
                if !self.objectives.contains_key(&name) && self.objectives.len() >= MAX_OBJECTIVES {
                    return false;
                }
                self.objectives
                    .insert(name.clone(), definition.into_objective(name.clone()));
                true
            }),
            ObjectiveAction::Update => definition.is_some_and(|definition| {
                let Some(objective) = self.objectives.get_mut(&name) else {
                    return false;
                };
                *objective = definition.into_objective(name.clone());
                true
            }),
            ObjectiveAction::Remove => {
                let removed = self.objectives.remove(&name).is_some();
                let before_slots = self.display_slots.len();
                self.display_slots.retain(|_, objective| objective != &name);
                detached_slots = before_slots - self.display_slots.len();
                let before_scores = self.scores.len();
                self.scores.retain(|key, _| key.objective != name);
                removed_scores = before_scores - self.scores.len();
                removed || detached_slots != 0 || removed_scores != 0
            }
            ObjectiveAction::Unknown(_) => false,
        };
        ScoreboardEvent::ObjectiveChanged {
            name: name.clone(),
            action,
            current: self.objectives.get(&name).cloned().map(Box::new),
            detached_slots,
            removed_scores,
            applied,
        }
    }

    fn apply_display_slot(
        &mut self,
        slot: DisplaySlot,
        objective: Option<String>,
    ) -> ScoreboardEvent {
        let applied = match objective.as_ref() {
            None => self.display_slots.remove(&slot).is_some(),
            Some(name) if !self.objectives.contains_key(name) => false,
            Some(name) => {
                if !self.display_slots.contains_key(&slot)
                    && self.display_slots.len() >= MAX_DISPLAY_SLOTS
                {
                    false
                } else {
                    self.display_slots.insert(slot, name.clone());
                    true
                }
            }
        };
        ScoreboardEvent::DisplaySlotChanged {
            slot,
            objective: self.display_slots.get(&slot).cloned(),
            applied,
        }
    }

    fn apply_score(&mut self, score: Score) -> ScoreboardEvent {
        let key = ScoreKey::new(score.owner.clone(), score.objective.clone());
        let objective_exists = self.objectives.contains_key(&score.objective);
        let has_capacity = self.scores.contains_key(&key) || self.scores.len() < MAX_SCORES;
        let applied = if objective_exists && has_capacity {
            self.scores.insert(key.clone(), score);
            true
        } else {
            false
        };
        ScoreboardEvent::ScoreChanged {
            owner: key.owner.clone(),
            objective: Some(key.objective.clone()),
            action: ScoreAction::Set,
            current: self.scores.get(&key).cloned().map(Box::new),
            affected: usize::from(applied),
            applied,
        }
    }

    fn apply_score_reset(&mut self, owner: String, objective: Option<String>) -> ScoreboardEvent {
        let affected = if let Some(objective) = objective.as_ref() {
            usize::from(
                self.scores
                    .remove(&ScoreKey::new(owner.clone(), objective.clone()))
                    .is_some(),
            )
        } else {
            let before = self.scores.len();
            self.scores.retain(|key, _| key.owner != owner);
            before - self.scores.len()
        };
        ScoreboardEvent::ScoreChanged {
            owner,
            objective,
            action: ScoreAction::Reset,
            current: None,
            affected,
            applied: affected != 0,
        }
    }

    fn apply_team(&mut self, update: TeamUpdate) -> ScoreboardEvent {
        let name = update.name().to_string();
        let action = update.action();
        let (applied, affected_members, rejected_members) = match update {
            TeamUpdate::Create {
                name,
                definition,
                members,
            } => {
                if !self.teams.contains_key(&name) && self.teams.len() >= MAX_TEAMS {
                    (false, 0, members.len())
                } else {
                    self.remove_team_members(&name);
                    self.teams
                        .insert(name.clone(), definition.into_team(name.clone()));
                    let (affected, rejected) = self.add_team_members(&name, members);
                    (true, affected, rejected)
                }
            }
            TeamUpdate::Remove { name } => {
                let affected = self.remove_team_members(&name);
                (self.teams.remove(&name).is_some(), affected, 0)
            }
            TeamUpdate::Update { name, definition } => {
                let Some(team) = self.teams.get_mut(&name) else {
                    return self.team_event(name, action, false, 0, 0);
                };
                let members = std::mem::take(&mut team.members);
                *team = definition.into_team(name.clone());
                team.members = members;
                (true, 0, 0)
            }
            TeamUpdate::AddMembers { name, members } => {
                if !self.teams.contains_key(&name) {
                    (false, 0, members.len())
                } else {
                    let (affected, rejected) = self.add_team_members(&name, members);
                    (affected != 0, affected, rejected)
                }
            }
            TeamUpdate::RemoveMembers { name, members } => {
                if !self.teams.contains_key(&name) {
                    (false, 0, members.len())
                } else {
                    let affected = self.remove_named_team_members(&name, members);
                    (affected != 0, affected, 0)
                }
            }
            TeamUpdate::Unknown { .. } => (false, 0, 0),
        };
        self.team_event(name, action, applied, affected_members, rejected_members)
    }

    fn team_event(
        &self,
        name: String,
        action: TeamAction,
        applied: bool,
        affected_members: usize,
        rejected_members: usize,
    ) -> ScoreboardEvent {
        ScoreboardEvent::TeamChanged {
            current: self.teams.get(&name).cloned().map(Box::new),
            name,
            action,
            affected_members,
            rejected_members,
            applied,
        }
    }

    fn add_team_members(&mut self, team_name: &str, members: Vec<String>) -> (usize, usize) {
        let mut affected = 0;
        let mut rejected = 0;
        for member in members {
            if self
                .member_teams
                .get(&member)
                .is_some_and(|team| team == team_name)
            {
                continue;
            }
            if !self.member_teams.contains_key(&member)
                && self.member_teams.len() >= MAX_TEAM_MEMBERS
            {
                rejected += 1;
                continue;
            }
            if let Some(previous) = self
                .member_teams
                .insert(member.clone(), team_name.to_string())
            {
                if let Some(team) = self.teams.get_mut(&previous) {
                    team.members.remove(&member);
                }
            }
            if let Some(team) = self.teams.get_mut(team_name) {
                team.members.insert(member);
                affected += 1;
            }
        }
        (affected, rejected)
    }

    fn remove_team_members(&mut self, team_name: &str) -> usize {
        let Some(team) = self.teams.get(team_name) else {
            return 0;
        };
        let members = team.members.iter().cloned().collect::<Vec<_>>();
        let affected = members.len();
        for member in members {
            if self
                .member_teams
                .get(&member)
                .is_some_and(|team| team == team_name)
            {
                self.member_teams.remove(&member);
            }
        }
        if let Some(team) = self.teams.get_mut(team_name) {
            team.members.clear();
        }
        affected
    }

    fn remove_named_team_members(&mut self, team_name: &str, members: Vec<String>) -> usize {
        let mut affected = 0;
        for member in members {
            if !self
                .member_teams
                .get(&member)
                .is_some_and(|team| team == team_name)
            {
                continue;
            }
            self.member_teams.remove(&member);
            if let Some(team) = self.teams.get_mut(team_name) {
                team.members.remove(&member);
            }
            affected += 1;
        }
        affected
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Objective {
    pub name: String,
    pub display_name: TextComponent,
    pub render_type: ObjectiveRenderType,
    /// `None` means vanilla's default integer formatting.
    pub number_format: Option<NumberFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectiveRenderType {
    Integer,
    Hearts,
    Unknown(i32),
}

impl From<i32> for ObjectiveRenderType {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Integer,
            1 => Self::Hearts,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NumberFormat {
    Blank,
    /// The raw style compound is retained for a future renderer.
    Styled(Nbt),
    Fixed(Box<TextComponent>),
    Unknown {
        kind: i32,
        styling: Option<Nbt>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DisplaySlot {
    List,
    Sidebar,
    BelowName,
    Team(TeamColor),
    Unknown(i32),
}

impl From<i32> for DisplaySlot {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::List,
            1 => Self::Sidebar,
            2 => Self::BelowName,
            3..=18 => Self::Team(TeamColor::from(value - 3)),
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScoreKey {
    pub objective: String,
    pub owner: String,
}

impl ScoreKey {
    pub fn new(owner: String, objective: String) -> Self {
        Self { objective, owner }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub owner: String,
    pub objective: String,
    pub value: i32,
    pub display_name: Option<TextComponent>,
    /// `None` inherits the objective's number format.
    pub number_format: Option<NumberFormat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Team {
    pub name: String,
    pub display_name: TextComponent,
    pub friendly_fire: FriendlyFire,
    pub name_tag_visibility: NameTagVisibility,
    pub collision_rule: CollisionRule,
    pub color: TeamColor,
    pub prefix: TextComponent,
    pub suffix: TextComponent,
    pub members: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FriendlyFire {
    pub raw: u8,
}

impl FriendlyFire {
    pub fn allows_friendly_fire(self) -> bool {
        self.raw & 0x01 != 0
    }

    pub fn see_friendly_invisibles(self) -> bool {
        self.raw & 0x02 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameTagVisibility {
    Always,
    Never,
    HideForOtherTeams,
    HideForOwnTeam,
    Unknown(String),
}

impl From<String> for NameTagVisibility {
    fn from(value: String) -> Self {
        match value.as_str() {
            "always" => Self::Always,
            "never" => Self::Never,
            "hideForOtherTeams" => Self::HideForOtherTeams,
            "hideForOwnTeam" => Self::HideForOwnTeam,
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionRule {
    Always,
    Never,
    PushOtherTeams,
    PushOwnTeam,
    Unknown(String),
}

impl From<String> for CollisionRule {
    fn from(value: String) -> Self {
        match value.as_str() {
            "always" => Self::Always,
            "never" => Self::Never,
            "pushOtherTeams" => Self::PushOtherTeams,
            "pushOwnTeam" => Self::PushOwnTeam,
            _ => Self::Unknown(value),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TeamColor {
    Black,
    DarkBlue,
    DarkGreen,
    DarkAqua,
    DarkRed,
    DarkPurple,
    Gold,
    Gray,
    DarkGray,
    Blue,
    Green,
    Aqua,
    Red,
    LightPurple,
    Yellow,
    White,
    Reset,
    Unknown(i32),
}

impl From<i32> for TeamColor {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Black,
            1 => Self::DarkBlue,
            2 => Self::DarkGreen,
            3 => Self::DarkAqua,
            4 => Self::DarkRed,
            5 => Self::DarkPurple,
            6 => Self::Gold,
            7 => Self::Gray,
            8 => Self::DarkGray,
            9 => Self::Blue,
            10 => Self::Green,
            11 => Self::Aqua,
            12 => Self::Red,
            13 => Self::LightPurple,
            14 => Self::Yellow,
            15 => Self::White,
            -1 => Self::Reset,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectiveAction {
    Create,
    Remove,
    Update,
    Unknown(i8),
}

impl From<i8> for ObjectiveAction {
    fn from(value: i8) -> Self {
        match value {
            0 => Self::Create,
            1 => Self::Remove,
            2 => Self::Update,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreAction {
    Set,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamAction {
    Create,
    Remove,
    Update,
    AddMembers,
    RemoveMembers,
    Unknown(i8),
}

/// Ordered scoreboard event carried by [`crate::minecraft::event::BotEvent`].
#[derive(Debug, Clone, PartialEq)]
pub enum ScoreboardEvent {
    ObjectiveChanged {
        name: String,
        action: ObjectiveAction,
        current: Option<Box<Objective>>,
        detached_slots: usize,
        removed_scores: usize,
        applied: bool,
    },
    DisplaySlotChanged {
        slot: DisplaySlot,
        objective: Option<String>,
        applied: bool,
    },
    ScoreChanged {
        owner: String,
        objective: Option<String>,
        action: ScoreAction,
        current: Option<Box<Score>>,
        affected: usize,
        applied: bool,
    },
    TeamChanged {
        name: String,
        action: TeamAction,
        current: Option<Box<Team>>,
        affected_members: usize,
        rejected_members: usize,
        applied: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum ScoreboardUpdate {
    Objective {
        name: String,
        action: ObjectiveAction,
        definition: Option<Box<ObjectiveDefinition>>,
    },
    DisplaySlot {
        slot: DisplaySlot,
        objective: Option<String>,
    },
    SetScore(Box<Score>),
    ResetScore {
        owner: String,
        objective: Option<String>,
    },
    Team(Box<TeamUpdate>),
}

#[derive(Debug, Clone)]
pub(crate) struct ObjectiveDefinition {
    display_name: TextComponent,
    render_type: ObjectiveRenderType,
    number_format: Option<NumberFormat>,
}

impl ObjectiveDefinition {
    fn into_objective(self, name: String) -> Objective {
        Objective {
            name,
            display_name: self.display_name,
            render_type: self.render_type,
            number_format: self.number_format,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TeamDefinition {
    display_name: TextComponent,
    friendly_fire: FriendlyFire,
    name_tag_visibility: NameTagVisibility,
    collision_rule: CollisionRule,
    color: TeamColor,
    prefix: TextComponent,
    suffix: TextComponent,
}

impl TeamDefinition {
    fn into_team(self, name: String) -> Team {
        Team {
            name,
            display_name: self.display_name,
            friendly_fire: self.friendly_fire,
            name_tag_visibility: self.name_tag_visibility,
            collision_rule: self.collision_rule,
            color: self.color,
            prefix: self.prefix,
            suffix: self.suffix,
            members: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum TeamUpdate {
    Create {
        name: String,
        definition: Box<TeamDefinition>,
        members: Vec<String>,
    },
    Remove {
        name: String,
    },
    Update {
        name: String,
        definition: Box<TeamDefinition>,
    },
    AddMembers {
        name: String,
        members: Vec<String>,
    },
    RemoveMembers {
        name: String,
        members: Vec<String>,
    },
    Unknown {
        name: String,
        mode: i8,
    },
}

impl TeamUpdate {
    fn name(&self) -> &str {
        match self {
            Self::Create { name, .. }
            | Self::Remove { name }
            | Self::Update { name, .. }
            | Self::AddMembers { name, .. }
            | Self::RemoveMembers { name, .. }
            | Self::Unknown { name, .. } => name,
        }
    }

    fn action(&self) -> TeamAction {
        match self {
            Self::Create { .. } => TeamAction::Create,
            Self::Remove { .. } => TeamAction::Remove,
            Self::Update { .. } => TeamAction::Update,
            Self::AddMembers { .. } => TeamAction::AddMembers,
            Self::RemoveMembers { .. } => TeamAction::RemoveMembers,
            Self::Unknown { mode, .. } => TeamAction::Unknown(*mode),
        }
    }
}

/// Decodes all protocol-769 scoreboard/team packets. `None` means another
/// subsystem owns the packet id.
pub(crate) fn decode_update(id: i32, payload: &[u8]) -> Result<Option<ScoreboardUpdate>> {
    let mut input = PacketReader::new(payload);
    let update = match id {
        CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID => {
            let packet = PacketScoreboardObjective::decode(&mut input)?;
            let action = ObjectiveAction::from(packet.action);
            let definition = objective_definition(&packet);
            ScoreboardUpdate::Objective {
                name: packet.name,
                action,
                definition: definition.map(Box::new),
            }
        }
        CLIENTBOUND_SCOREBOARD_DISPLAY_OBJECTIVE_ID => {
            let packet = PacketScoreboardDisplayObjective::decode(&mut input)?;
            ScoreboardUpdate::DisplaySlot {
                slot: DisplaySlot::from(packet.position),
                objective: (!packet.name.is_empty()).then_some(packet.name),
            }
        }
        CLIENTBOUND_SCOREBOARD_SCORE_ID => {
            let packet = PacketScoreboardScore::decode(&mut input)?;
            let styling = score_styling(packet.styling);
            ScoreboardUpdate::SetScore(Box::new(Score {
                owner: packet.item_name,
                objective: packet.score_name,
                value: packet.value,
                display_name: packet.display_name.as_ref().map(TextComponent::from_nbt),
                number_format: number_format(packet.number_format, styling),
            }))
        }
        CLIENTBOUND_RESET_SCORE_ID => {
            let packet = PacketResetScore::decode(&mut input)?;
            ScoreboardUpdate::ResetScore {
                owner: packet.entity_name,
                objective: packet.objective_name,
            }
        }
        CLIENTBOUND_TEAMS_ID => {
            let packet = PacketTeams::decode(&mut input)?;
            ScoreboardUpdate::Team(Box::new(team_update(packet)))
        }
        _ => return Ok(None),
    };
    Ok(Some(update))
}

fn objective_definition(packet: &PacketScoreboardObjective) -> Option<ObjectiveDefinition> {
    let display_name = match &packet.display_text {
        PacketScoreboardObjectiveDisplayText::V0(value)
        | PacketScoreboardObjectiveDisplayText::V2(value) => TextComponent::from_nbt(value),
        PacketScoreboardObjectiveDisplayText::Default => return None,
    };
    let render_type = match &packet.r#type {
        PacketScoreboardObjectiveType::V0(value) | PacketScoreboardObjectiveType::V2(value) => {
            ObjectiveRenderType::from(*value)
        }
        PacketScoreboardObjectiveType::Default => return None,
    };
    let kind = match &packet.number_format {
        PacketScoreboardObjectiveNumberFormat::V0(value)
        | PacketScoreboardObjectiveNumberFormat::V2(value) => *value,
        PacketScoreboardObjectiveNumberFormat::Default => return None,
    };
    Some(ObjectiveDefinition {
        display_name,
        render_type,
        number_format: number_format(kind, objective_styling(&packet.styling)),
    })
}

fn objective_styling(styling: &PacketScoreboardObjectiveStyling) -> Option<Nbt> {
    match styling {
        PacketScoreboardObjectiveStyling::V0(value) => match value {
            PacketScoreboardObjectiveStylingV0::V1(value)
            | PacketScoreboardObjectiveStylingV0::V2(value) => Some(value.clone()),
            PacketScoreboardObjectiveStylingV0::Default => None,
        },
        PacketScoreboardObjectiveStyling::V2(value) => match value {
            PacketScoreboardObjectiveStylingV2::V1(value)
            | PacketScoreboardObjectiveStylingV2::V2(value) => Some(value.clone()),
            PacketScoreboardObjectiveStylingV2::Default => None,
        },
        PacketScoreboardObjectiveStyling::Default => None,
    }
}

fn score_styling(styling: PacketScoreboardScoreStyling) -> Option<Nbt> {
    match styling {
        PacketScoreboardScoreStyling::V1(value) | PacketScoreboardScoreStyling::V2(value) => {
            Some(value)
        }
        PacketScoreboardScoreStyling::Default => None,
    }
}

fn number_format(kind: Option<i32>, styling: Option<Nbt>) -> Option<NumberFormat> {
    match kind {
        None => None,
        Some(0) => Some(NumberFormat::Blank),
        Some(1) => Some(match styling {
            Some(styling) => NumberFormat::Styled(styling),
            None => NumberFormat::Unknown {
                kind: 1,
                styling: None,
            },
        }),
        Some(2) => Some(match styling {
            Some(styling) => NumberFormat::Fixed(Box::new(TextComponent::from_nbt(&styling))),
            None => NumberFormat::Unknown {
                kind: 2,
                styling: None,
            },
        }),
        Some(kind) => Some(NumberFormat::Unknown { kind, styling }),
    }
}

fn team_update(packet: PacketTeams) -> TeamUpdate {
    let PacketTeams {
        team,
        mode,
        name,
        friendly_fire,
        name_tag_visibility,
        collision_rule,
        formatting,
        prefix,
        suffix,
        players,
    } = packet;
    match mode {
        0 => match (
            team_definition(
                name,
                friendly_fire,
                name_tag_visibility,
                collision_rule,
                formatting,
                prefix,
                suffix,
            ),
            team_players(players),
        ) {
            (Some(definition), Some(members)) => TeamUpdate::Create {
                name: team,
                definition: Box::new(definition),
                members,
            },
            _ => TeamUpdate::Unknown { name: team, mode },
        },
        1 => TeamUpdate::Remove { name: team },
        2 => match team_definition(
            name,
            friendly_fire,
            name_tag_visibility,
            collision_rule,
            formatting,
            prefix,
            suffix,
        ) {
            Some(definition) => TeamUpdate::Update {
                name: team,
                definition: Box::new(definition),
            },
            None => TeamUpdate::Unknown { name: team, mode },
        },
        3 => match team_players(players) {
            Some(members) => TeamUpdate::AddMembers {
                name: team,
                members,
            },
            None => TeamUpdate::Unknown { name: team, mode },
        },
        4 => match team_players(players) {
            Some(members) => TeamUpdate::RemoveMembers {
                name: team,
                members,
            },
            None => TeamUpdate::Unknown { name: team, mode },
        },
        _ => TeamUpdate::Unknown { name: team, mode },
    }
}

#[allow(clippy::too_many_arguments)]
fn team_definition(
    name: PacketTeamsName,
    friendly_fire: PacketTeamsFriendlyFire,
    name_tag_visibility: PacketTeamsNameTagVisibility,
    collision_rule: PacketTeamsCollisionRule,
    formatting: PacketTeamsFormatting,
    prefix: PacketTeamsPrefix,
    suffix: PacketTeamsSuffix,
) -> Option<TeamDefinition> {
    let display_name = match name {
        PacketTeamsName::V0(value) | PacketTeamsName::V2(value) => TextComponent::from_nbt(&value),
        PacketTeamsName::Default => return None,
    };
    let friendly_fire = match friendly_fire {
        PacketTeamsFriendlyFire::V0(value) | PacketTeamsFriendlyFire::V2(value) => {
            FriendlyFire { raw: value as u8 }
        }
        PacketTeamsFriendlyFire::Default => return None,
    };
    let name_tag_visibility = match name_tag_visibility {
        PacketTeamsNameTagVisibility::V0(value) | PacketTeamsNameTagVisibility::V2(value) => {
            NameTagVisibility::from(value)
        }
        PacketTeamsNameTagVisibility::Default => return None,
    };
    let collision_rule = match collision_rule {
        PacketTeamsCollisionRule::V0(value) | PacketTeamsCollisionRule::V2(value) => {
            CollisionRule::from(value)
        }
        PacketTeamsCollisionRule::Default => return None,
    };
    let color = match formatting {
        PacketTeamsFormatting::V0(value) | PacketTeamsFormatting::V2(value) => {
            TeamColor::from(value)
        }
        PacketTeamsFormatting::Default => return None,
    };
    let prefix = match prefix {
        PacketTeamsPrefix::V0(value) | PacketTeamsPrefix::V2(value) => {
            TextComponent::from_nbt(&value)
        }
        PacketTeamsPrefix::Default => return None,
    };
    let suffix = match suffix {
        PacketTeamsSuffix::V0(value) | PacketTeamsSuffix::V2(value) => {
            TextComponent::from_nbt(&value)
        }
        PacketTeamsSuffix::Default => return None,
    };
    Some(TeamDefinition {
        display_name,
        friendly_fire,
        name_tag_visibility,
        collision_rule,
        color,
        prefix,
        suffix,
    })
}

fn team_players(players: PacketTeamsPlayers) -> Option<Vec<String>> {
    match players {
        PacketTeamsPlayers::V0(players)
        | PacketTeamsPlayers::V3(players)
        | PacketTeamsPlayers::V4(players) => Some(players),
        PacketTeamsPlayers::Default => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::buffer::PacketWriter;
    use minerider_protocol::traits::Encode;

    fn text(value: &str) -> Nbt {
        Nbt::Compound(vec![("text".into(), Nbt::String(value.into()))])
    }

    fn apply_packet<T: Encode>(
        state: &mut ScoreboardState,
        id: i32,
        packet: &T,
    ) -> ScoreboardEvent {
        let mut output = PacketWriter::new();
        packet.encode(&mut output).unwrap();
        let update = decode_update(id, &output.into_inner())
            .unwrap()
            .expect("scoreboard packet");
        state.apply(update)
    }

    fn objective_packet(name: &str, action: i8) -> PacketScoreboardObjective {
        PacketScoreboardObjective {
            name: name.into(),
            action,
            display_text: PacketScoreboardObjectiveDisplayText::Default,
            r#type: PacketScoreboardObjectiveType::Default,
            number_format: PacketScoreboardObjectiveNumberFormat::Default,
            styling: PacketScoreboardObjectiveStyling::Default,
        }
    }

    fn create_objective(name: &str, label: &str) -> PacketScoreboardObjective {
        PacketScoreboardObjective {
            name: name.into(),
            action: 0,
            display_text: PacketScoreboardObjectiveDisplayText::V0(text(label)),
            r#type: PacketScoreboardObjectiveType::V0(0),
            number_format: PacketScoreboardObjectiveNumberFormat::V0(None),
            styling: PacketScoreboardObjectiveStyling::V0(
                PacketScoreboardObjectiveStylingV0::Default,
            ),
        }
    }

    fn team_packet(name: &str, mode: i8) -> PacketTeams {
        PacketTeams {
            team: name.into(),
            mode,
            name: PacketTeamsName::Default,
            friendly_fire: PacketTeamsFriendlyFire::Default,
            name_tag_visibility: PacketTeamsNameTagVisibility::Default,
            collision_rule: PacketTeamsCollisionRule::Default,
            formatting: PacketTeamsFormatting::Default,
            prefix: PacketTeamsPrefix::Default,
            suffix: PacketTeamsSuffix::Default,
            players: PacketTeamsPlayers::Default,
        }
    }

    fn create_team(name: &str, members: Vec<String>, color: i32) -> PacketTeams {
        PacketTeams {
            team: name.into(),
            mode: 0,
            name: PacketTeamsName::V0(text(name)),
            friendly_fire: PacketTeamsFriendlyFire::V0(0x03),
            name_tag_visibility: PacketTeamsNameTagVisibility::V0("always".into()),
            collision_rule: PacketTeamsCollisionRule::V0("pushOtherTeams".into()),
            formatting: PacketTeamsFormatting::V0(color),
            prefix: PacketTeamsPrefix::V0(text("[")),
            suffix: PacketTeamsSuffix::V0(text("]")),
            players: PacketTeamsPlayers::V0(members),
        }
    }

    #[test]
    fn objective_display_score_and_removal_have_complete_lifecycle() {
        let mut state = ScoreboardState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
            &create_objective("kills", "Kills"),
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_DISPLAY_OBJECTIVE_ID,
            &PacketScoreboardDisplayObjective {
                position: 1,
                name: "kills".into(),
            },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_SCORE_ID,
            &PacketScoreboardScore {
                item_name: "Alice".into(),
                score_name: "kills".into(),
                value: 7,
                display_name: Some(text("Alice the Brave")),
                number_format: Some(2),
                styling: PacketScoreboardScoreStyling::V2(text("seven")),
            },
        );

        assert_eq!(state.display_slots[&DisplaySlot::Sidebar], "kills");
        let score = &state.scores[&ScoreKey::new("Alice".into(), "kills".into())];
        assert_eq!(score.value, 7);
        assert!(matches!(
            score.number_format.as_ref(),
            Some(NumberFormat::Fixed(_))
        ));

        let mut update = objective_packet("kills", 2);
        update.display_text = PacketScoreboardObjectiveDisplayText::V2(text("Eliminations"));
        update.r#type = PacketScoreboardObjectiveType::V2(1);
        update.number_format = PacketScoreboardObjectiveNumberFormat::V2(Some(0));
        update.styling =
            PacketScoreboardObjectiveStyling::V2(PacketScoreboardObjectiveStylingV2::Default);
        apply_packet(&mut state, CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID, &update);
        assert_eq!(
            state.objectives["kills"].render_type,
            ObjectiveRenderType::Hearts
        );
        assert_eq!(
            state.objectives["kills"].number_format,
            Some(NumberFormat::Blank)
        );

        let removed = apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
            &objective_packet("kills", 1),
        );
        assert!(matches!(
            removed,
            ScoreboardEvent::ObjectiveChanged {
                detached_slots: 1,
                removed_scores: 1,
                applied: true,
                ..
            }
        ));
        assert!(state.objectives.is_empty());
        assert!(state.display_slots.is_empty());
        assert!(state.scores.is_empty());
    }

    #[test]
    fn score_reset_can_target_one_objective_or_every_objective() {
        let mut state = ScoreboardState::default();
        for objective in ["a", "b"] {
            apply_packet(
                &mut state,
                CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
                &create_objective(objective, objective),
            );
            apply_packet(
                &mut state,
                CLIENTBOUND_SCOREBOARD_SCORE_ID,
                &PacketScoreboardScore {
                    item_name: "Alice".into(),
                    score_name: objective.into(),
                    value: 1,
                    display_name: None,
                    number_format: None,
                    styling: PacketScoreboardScoreStyling::Default,
                },
            );
        }
        apply_packet(
            &mut state,
            CLIENTBOUND_RESET_SCORE_ID,
            &PacketResetScore {
                entity_name: "Alice".into(),
                objective_name: Some("a".into()),
            },
        );
        assert_eq!(state.scores.len(), 1);
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_RESET_SCORE_ID,
            &PacketResetScore {
                entity_name: "Alice".into(),
                objective_name: None,
            },
        );
        assert!(matches!(
            event,
            ScoreboardEvent::ScoreChanged { affected: 1, .. }
        ));
        assert!(state.scores.is_empty());
    }

    #[test]
    fn team_lifecycle_preserves_options_and_moves_members_atomically() {
        let mut state = ScoreboardState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_TEAMS_ID,
            &create_team("red", vec!["Alice".into(), "Bob".into()], 99),
        );
        let red = &state.teams["red"];
        assert_eq!(red.color, TeamColor::Unknown(99));
        assert!(red.friendly_fire.allows_friendly_fire());
        assert!(red.friendly_fire.see_friendly_invisibles());
        assert_eq!(red.name_tag_visibility, NameTagVisibility::Always);
        assert_eq!(red.collision_rule, CollisionRule::PushOtherTeams);

        apply_packet(
            &mut state,
            CLIENTBOUND_TEAMS_ID,
            &create_team("blue", vec!["Alice".into()], 9),
        );
        assert_eq!(state.team_for_member("Alice"), Some("blue"));
        assert!(!state.teams["red"].members.contains("Alice"));

        let mut update = team_packet("red", 2);
        update.name = PacketTeamsName::V2(text("Updated Red"));
        update.friendly_fire = PacketTeamsFriendlyFire::V2(0);
        update.name_tag_visibility = PacketTeamsNameTagVisibility::V2("future_visibility".into());
        update.collision_rule = PacketTeamsCollisionRule::V2("never".into());
        update.formatting = PacketTeamsFormatting::V2(12);
        update.prefix = PacketTeamsPrefix::V2(text("<"));
        update.suffix = PacketTeamsSuffix::V2(text(">"));
        apply_packet(&mut state, CLIENTBOUND_TEAMS_ID, &update);
        assert!(matches!(
            &state.teams["red"].name_tag_visibility,
            NameTagVisibility::Unknown(_)
        ));
        assert!(state.teams["red"].members.contains("Bob"));

        let mut add = team_packet("red", 3);
        add.players = PacketTeamsPlayers::V3(vec!["Cara".into()]);
        apply_packet(&mut state, CLIENTBOUND_TEAMS_ID, &add);
        let mut remove_members = team_packet("red", 4);
        remove_members.players = PacketTeamsPlayers::V4(vec!["Bob".into()]);
        apply_packet(&mut state, CLIENTBOUND_TEAMS_ID, &remove_members);
        assert!(state.teams["red"].members.contains("Cara"));
        assert!(!state.teams["red"].members.contains("Bob"));

        apply_packet(&mut state, CLIENTBOUND_TEAMS_ID, &team_packet("red", 1));
        assert!(!state.teams.contains_key("red"));
        assert_eq!(state.team_for_member("Cara"), None);
    }

    #[test]
    fn out_of_order_and_unknown_actions_are_safe_noops() {
        let mut state = ScoreboardState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_DISPLAY_OBJECTIVE_ID,
            &PacketScoreboardDisplayObjective {
                position: 50,
                name: "missing".into(),
            },
        );
        assert!(matches!(
            event,
            ScoreboardEvent::DisplaySlotChanged {
                slot: DisplaySlot::Unknown(50),
                applied: false,
                ..
            }
        ));
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
            &objective_packet("missing", 2),
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_SCORE_ID,
            &PacketScoreboardScore {
                item_name: "Alice".into(),
                score_name: "missing".into(),
                value: 1,
                display_name: None,
                number_format: Some(77),
                styling: PacketScoreboardScoreStyling::Default,
            },
        );
        let team_event = apply_packet(&mut state, CLIENTBOUND_TEAMS_ID, &team_packet("ghost", 99));
        assert!(matches!(
            team_event,
            ScoreboardEvent::TeamChanged {
                action: TeamAction::Unknown(99),
                applied: false,
                ..
            }
        ));
        assert!(state.objectives.is_empty());
        assert!(state.scores.is_empty());
        assert!(state.teams.is_empty());
    }

    #[test]
    fn unknown_number_format_is_preserved() {
        let mut state = ScoreboardState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID,
            &create_objective("o", "O"),
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SCOREBOARD_SCORE_ID,
            &PacketScoreboardScore {
                item_name: "Alice".into(),
                score_name: "o".into(),
                value: 1,
                display_name: None,
                number_format: Some(77),
                styling: PacketScoreboardScoreStyling::Default,
            },
        );
        assert!(matches!(
            state.scores.values().next().unwrap().number_format.as_ref(),
            Some(NumberFormat::Unknown {
                kind: 77,
                styling: None
            })
        ));
    }

    #[test]
    fn collections_are_bounded_and_ordered() {
        let mut state = ScoreboardState::default();
        for index in (0..MAX_OBJECTIVES + 1).rev() {
            state.apply(ScoreboardUpdate::Objective {
                name: format!("o{index:04}"),
                action: ObjectiveAction::Create,
                definition: Some(Box::new(ObjectiveDefinition {
                    display_name: TextComponent::literal(index.to_string()),
                    render_type: ObjectiveRenderType::Integer,
                    number_format: None,
                })),
            });
        }
        assert_eq!(state.objectives.len(), MAX_OBJECTIVES);
        assert!(state.objectives.keys().is_sorted());

        let objective = state.objectives.keys().next().unwrap().clone();
        for index in 0..=MAX_SCORES {
            state.apply(ScoreboardUpdate::SetScore(Box::new(Score {
                owner: format!("p{index:05}"),
                objective: objective.clone(),
                value: index as i32,
                display_name: None,
                number_format: None,
            })));
        }
        assert_eq!(state.scores.len(), MAX_SCORES);

        for slot in 100..=(100 + MAX_DISPLAY_SLOTS as i32) {
            state.apply(ScoreboardUpdate::DisplaySlot {
                slot: DisplaySlot::Unknown(slot),
                objective: Some(objective.clone()),
            });
        }
        assert_eq!(state.display_slots.len(), MAX_DISPLAY_SLOTS);
    }

    #[test]
    fn team_and_member_collections_are_bounded() {
        let mut state = ScoreboardState::default();
        let definition = || TeamDefinition {
            display_name: TextComponent::literal("team"),
            friendly_fire: FriendlyFire { raw: 0 },
            name_tag_visibility: NameTagVisibility::Always,
            collision_rule: CollisionRule::Always,
            color: TeamColor::Reset,
            prefix: TextComponent::default(),
            suffix: TextComponent::default(),
        };
        for index in 0..=MAX_TEAMS {
            state.apply(ScoreboardUpdate::Team(Box::new(TeamUpdate::Create {
                name: format!("t{index:04}"),
                definition: Box::new(definition()),
                members: Vec::new(),
            })));
        }
        assert_eq!(state.teams.len(), MAX_TEAMS);

        let members = (0..=MAX_TEAM_MEMBERS)
            .map(|index| format!("m{index:05}"))
            .collect();
        let event = state.apply(ScoreboardUpdate::Team(Box::new(TeamUpdate::AddMembers {
            name: "t0000".into(),
            members,
        })));
        assert_eq!(state.teams["t0000"].members.len(), MAX_TEAM_MEMBERS);
        assert!(matches!(
            event,
            ScoreboardEvent::TeamChanged {
                rejected_members: 1,
                ..
            }
        ));
    }

    #[test]
    fn malformed_payload_is_a_protocol_error() {
        assert!(decode_update(CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID, &[0x80]).is_err());
    }
}
