//! Packet coverage classification and vanilla-conformance obligations.
//!
//! Every clientbound packet in `login`, `configuration` and `play` is
//! explicitly classified here. No packet may disappear silently: packets
//! without a detailed entry fall back to [`CoverageClass::IntentionallyIgnored`]
//! with [`ConformanceStatus::NotImplemented`], and the play/configuration
//! loops log a structured warning for anything that is not `Handled`.
//!
//! Statuses are honest: nothing is `Pass` until a real vanilla 1.21.4
//! capture exists and the semantic diff agrees. Mock-server + golden tests
//! only justify `Partial`.

use crate::core::state::ConnectionState;
use minerider_protocol::generated::v1_21_4::{configuration, login, play};

/// How MineRider treats a clientbound packet today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageClass {
    /// Decoded and actively handled (response sent and/or state updated).
    Handled,
    /// Decoded and deliberately ignored; no response is required.
    IntentionallyIgnored,
    /// Decoded and retained for a later behavior system (world, entities…).
    StoredForLater,
    /// Not supported yet; must fail loudly (structured diagnostic).
    Unsupported,
}

/// Timing tolerance class used by the trace diff engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingClass {
    /// Required immediate acknowledgement; ordering must match exactly.
    Strict,
    /// Must happen within the same client tick as the trigger.
    TickBound,
    /// Periodic cadence (keep-alive, idle movement refresh).
    Periodic,
    /// Non-essential; compared loosely.
    BestEffort,
    /// No response expected.
    None,
}

/// Vanilla-conformance status of a packet behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceStatus {
    /// Vanilla trace + MineRider trace + semantic diff all agree.
    Pass,
    /// Implemented and covered by mock/golden tests; vanilla capture pending.
    Partial,
    /// Implemented but known to diverge from vanilla.
    Fail,
    /// Not implemented yet.
    NotImplemented,
    /// Vanilla has no observable obligation for this packet.
    NotApplicable,
}

/// What vanilla 1.21.4 does in response to a clientbound packet.
#[derive(Debug, Clone, Copy)]
pub struct Obligation {
    /// Serverbound packet vanilla sends in response, if any (generated name).
    pub responds_with: Option<&'static str>,
    /// Timing tolerance class of that response.
    pub timing: TimingClass,
    /// Required client state update, or "none".
    pub state_update: &'static str,
    /// Current MineRider conformance status.
    pub status: ConformanceStatus,
    /// Conformance scenario covering this packet, or "".
    pub scenario: &'static str,
    /// Evidence: test/capture reference. "none" means no coverage at all.
    pub evidence: &'static str,
}

/// Coverage entry for one clientbound packet.
#[derive(Debug, Clone, Copy)]
pub struct CoverageEntry {
    pub class: CoverageClass,
    pub obligation: Obligation,
}

const EVIDENCE_MOCK: &str =
    "mock-server + golden tests + Paper 1.21.4 b232; vanilla capture pending";
const EVIDENCE_VALIDATED: &str =
    "mock + Paper 1.21.4 b232 + vanilla 1.21.4 server; vanilla client capture pending";
const EVIDENCE_UNIT: &str = "unit-tested state projection; mock/vanilla capture pending";
const EVIDENCE_NONE: &str = "none";

const NO_RESPONSE: Obligation = Obligation {
    responds_with: None,
    timing: TimingClass::None,
    state_update: "none",
    status: ConformanceStatus::NotApplicable,
    scenario: "",
    evidence: EVIDENCE_NONE,
};

/// Fallback for packets without a detailed entry: decoded by generated code,
/// but no behavior implemented yet.
const DEFAULT_ENTRY: CoverageEntry = CoverageEntry {
    class: CoverageClass::IntentionallyIgnored,
    obligation: Obligation {
        status: ConformanceStatus::NotImplemented,
        ..NO_RESPONSE
    },
};

fn handled(obligation: Obligation) -> CoverageEntry {
    CoverageEntry {
        class: CoverageClass::Handled,
        obligation,
    }
}

fn ignored(obligation: Obligation) -> CoverageEntry {
    CoverageEntry {
        class: CoverageClass::IntentionallyIgnored,
        obligation,
    }
}

fn unsupported(responds_with: Option<&'static str>) -> CoverageEntry {
    CoverageEntry {
        class: CoverageClass::Unsupported,
        obligation: Obligation {
            responds_with,
            timing: TimingClass::Strict,
            status: ConformanceStatus::NotImplemented,
            ..NO_RESPONSE
        },
    }
}

fn partial(
    responds_with: Option<&'static str>,
    timing: TimingClass,
    state_update: &'static str,
    scenario: &'static str,
) -> Obligation {
    Obligation {
        responds_with,
        timing,
        state_update,
        status: ConformanceStatus::Partial,
        scenario,
        evidence: EVIDENCE_MOCK,
    }
}

fn not_implemented(
    responds_with: Option<&'static str>,
    timing: TimingClass,
    state_update: &'static str,
    scenario: &'static str,
) -> Obligation {
    Obligation {
        responds_with,
        timing,
        state_update,
        status: ConformanceStatus::NotImplemented,
        scenario,
        evidence: EVIDENCE_NONE,
    }
}

/// A state-only packet: decoded and folded into `PlayState`, no wire
/// response. Covered by unit tests; server-trace confirmation still pending.
fn state_only(state_update: &'static str) -> Obligation {
    Obligation {
        responds_with: None,
        timing: TimingClass::None,
        state_update,
        status: ConformanceStatus::Partial,
        scenario: "",
        evidence: EVIDENCE_UNIT,
    }
}

/// Looks up the coverage entry for a clientbound packet id in a state.
///
/// Every known id returns an entry; unknown ids (outside the generated
/// registry) return the default entry.
pub fn clientbound_coverage(state: ConnectionState, id: i32) -> CoverageEntry {
    match state {
        ConnectionState::Login => login_coverage(id),
        ConnectionState::Configuration => configuration_coverage(id),
        ConnectionState::Play => play_coverage(id),
        _ => DEFAULT_ENTRY,
    }
}

fn login_coverage(id: i32) -> CoverageEntry {
    match id {
        login::CLIENTBOUND_DISCONNECT_ID => handled(partial(
            None,
            TimingClass::None,
            "close connection",
            "offline_login",
        )),
        login::CLIENTBOUND_ENCRYPTION_BEGIN_ID => handled(partial(
            Some("encryption_begin"),
            TimingClass::Strict,
            "enable AES-CFB8 encryption",
            "offline_login",
        )),
        login::CLIENTBOUND_SUCCESS_ID => handled(partial(
            Some("login_acknowledged"),
            TimingClass::Strict,
            "transition to configuration",
            "offline_login",
        )),
        login::CLIENTBOUND_COMPRESS_ID => handled(partial(
            None,
            TimingClass::Strict,
            "enable zlib compression",
            "offline_login",
        )),
        login::CLIENTBOUND_LOGIN_PLUGIN_REQUEST_ID => handled(partial(
            Some("login_plugin_response"),
            TimingClass::Strict,
            "none",
            "offline_login",
        )),
        login::CLIENTBOUND_COOKIE_REQUEST_ID => unsupported(Some("cookie_response")),
        _ => DEFAULT_ENTRY,
    }
}

fn configuration_coverage(id: i32) -> CoverageEntry {
    match id {
        configuration::CLIENTBOUND_DISCONNECT_ID => handled(partial(
            None,
            TimingClass::None,
            "close connection",
            "configuration_completion",
        )),
        configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID => handled(partial(
            Some("finish_configuration"),
            TimingClass::Strict,
            "transition to play",
            "configuration_completion",
        )),
        configuration::CLIENTBOUND_KEEP_ALIVE_ID => handled(partial(
            Some("keep_alive"),
            TimingClass::Strict,
            "none",
            "join_idle",
        )),
        configuration::CLIENTBOUND_PING_ID => handled(partial(
            Some("pong"),
            TimingClass::Strict,
            "none",
            "join_idle",
        )),
        configuration::CLIENTBOUND_SELECT_KNOWN_PACKS_ID => handled(partial(
            Some("select_known_packs"),
            TimingClass::Strict,
            "none",
            "configuration_completion",
        )),
        configuration::CLIENTBOUND_CUSTOM_PAYLOAD_ID => ignored(Obligation {
            state_update: "vanilla sends brand voluntarily; no response required",
            status: ConformanceStatus::NotImplemented,
            evidence: EVIDENCE_NONE,
            ..NO_RESPONSE
        }),
        configuration::CLIENTBOUND_RESET_CHAT_ID => ignored(NO_RESPONSE),
        configuration::CLIENTBOUND_REGISTRY_DATA_ID => handled(state_only(
            "store dimension types for chunk decode and world physics",
        )),
        configuration::CLIENTBOUND_ADD_RESOURCE_PACK_ID => handled(Obligation {
            responds_with: Some("resource_pack_receive"),
            timing: TimingClass::Strict,
            state_update: "none",
            status: ConformanceStatus::Partial,
            scenario: "resource_pack",
            evidence: EVIDENCE_MOCK,
        }),
        configuration::CLIENTBOUND_REMOVE_RESOURCE_PACK_ID => {
            handled(state_only("no client-side pack state to remove"))
        }
        configuration::CLIENTBOUND_STORE_COOKIE_ID => ignored(Obligation {
            status: ConformanceStatus::NotImplemented,
            ..NO_RESPONSE
        }),
        configuration::CLIENTBOUND_TRANSFER_ID => ignored(not_implemented(
            None,
            TimingClass::Strict,
            "reconnect to another server",
            "",
        )),
        configuration::CLIENTBOUND_FEATURE_FLAGS_ID => ignored(not_implemented(
            None,
            TimingClass::None,
            "enable feature flags",
            "",
        )),
        configuration::CLIENTBOUND_TAGS_ID => CoverageEntry {
            class: CoverageClass::StoredForLater,
            obligation: not_implemented(None, TimingClass::None, "store tags", ""),
        },
        configuration::CLIENTBOUND_CUSTOM_REPORT_DETAILS_ID
        | configuration::CLIENTBOUND_SERVER_LINKS_ID => ignored(not_implemented(
            None,
            TimingClass::None,
            "store for pause-menu reporting",
            "",
        )),
        configuration::CLIENTBOUND_COOKIE_REQUEST_ID => unsupported(Some("cookie_response")),
        _ => DEFAULT_ENTRY,
    }
}

fn play_coverage(id: i32) -> CoverageEntry {
    match id {
        play::CLIENTBOUND_KEEP_ALIVE_ID => handled(Obligation {
            responds_with: Some("keep_alive"),
            timing: TimingClass::Strict,
            state_update: "none",
            status: ConformanceStatus::Partial,
            scenario: "join_idle",
            evidence: EVIDENCE_VALIDATED,
        }),
        play::CLIENTBOUND_KICK_DISCONNECT_ID => handled(partial(
            None,
            TimingClass::None,
            "store structured reason, emit event, close connection",
            "join_idle",
        )),
        play::CLIENTBOUND_LOGIN_ID => handled(partial(
            None,
            TimingClass::None,
            "store own entity id, dimension, world info",
            "join_idle",
        )),
        play::CLIENTBOUND_SPAWN_ENTITY_ID => handled(state_only("track new entity")),
        play::CLIENTBOUND_ENTITY_DESTROY_ID => handled(state_only("remove entities from tracker")),
        play::CLIENTBOUND_REL_ENTITY_MOVE_ID
        | play::CLIENTBOUND_ENTITY_MOVE_LOOK_ID
        | play::CLIENTBOUND_ENTITY_LOOK_ID
        | play::CLIENTBOUND_ENTITY_TELEPORT_ID
        | play::CLIENTBOUND_SYNC_ENTITY_POSITION_ID => {
            handled(state_only("update entity position/rotation"))
        }
        play::CLIENTBOUND_ENTITY_VELOCITY_ID => handled(state_only("update entity velocity")),
        play::CLIENTBOUND_ENTITY_HEAD_ROTATION_ID => handled(state_only("update entity head yaw")),
        play::CLIENTBOUND_ABILITIES_ID => handled(state_only(
            "store player ability flags and flying/walking speeds; emit HUD event",
        )),
        play::CLIENTBOUND_SET_COOLDOWN_ID => handled(state_only(
            "apply bounded cooldown group lifecycle; emit HUD event",
        )),
        play::CLIENTBOUND_ENTITY_EFFECT_ID | play::CLIENTBOUND_REMOVE_ENTITY_EFFECT_ID => handled(
            state_only("apply bounded local-player status-effect lifecycle; emit HUD event"),
        ),
        play::CLIENTBOUND_ENTITY_UPDATE_ATTRIBUTES_ID => handled(state_only(
            "replace bounded local-player attributes/modifiers; emit HUD event",
        )),
        play::CLIENTBOUND_EXPERIENCE_ID => handled(state_only("update experience bar/level/total")),
        play::CLIENTBOUND_DIFFICULTY_ID => handled(state_only(
            "store difficulty and server lock state; emit HUD event",
        )),
        play::CLIENTBOUND_SPAWN_POSITION_ID => handled(state_only(
            "store global spawn block position and angle; emit HUD event",
        )),
        play::CLIENTBOUND_INITIALIZE_WORLD_BORDER_ID
        | play::CLIENTBOUND_WORLD_BORDER_CENTER_ID
        | play::CLIENTBOUND_WORLD_BORDER_LERP_SIZE_ID
        | play::CLIENTBOUND_WORLD_BORDER_SIZE_ID
        | play::CLIENTBOUND_WORLD_BORDER_WARNING_DELAY_ID
        | play::CLIENTBOUND_WORLD_BORDER_WARNING_REACH_ID => handled(state_only(
            "apply bounded world-border lifecycle safely out of order; emit HUD event",
        )),
        play::CLIENTBOUND_POSITION_ID => handled(Obligation {
            responds_with: Some("teleport_confirm"),
            timing: TimingClass::Strict,
            state_update: "update position/rotation (relative flags applied)",
            status: ConformanceStatus::Partial,
            scenario: "teleport_correction",
            evidence: EVIDENCE_VALIDATED,
        }),
        play::CLIENTBOUND_CHUNK_BATCH_START_ID => handled(Obligation {
            responds_with: None,
            timing: TimingClass::None,
            state_update: "start timing the batch for adaptive chunks-per-tick pacing",
            status: ConformanceStatus::Partial,
            scenario: "initial_chunks",
            evidence: EVIDENCE_UNIT,
        }),
        play::CLIENTBOUND_CHUNK_BATCH_FINISHED_ID => handled(Obligation {
            responds_with: Some("chunk_batch_received"),
            timing: TimingClass::Strict,
            state_update: "acknowledge batch with desired chunks-per-tick",
            status: ConformanceStatus::Partial,
            scenario: "initial_chunks",
            evidence: EVIDENCE_VALIDATED,
        }),
        play::CLIENTBOUND_MAP_CHUNK_ID => handled(Obligation {
            responds_with: None,
            timing: TimingClass::None,
            state_update: "decode and store all block-state/biome chunk sections",
            status: ConformanceStatus::Partial,
            scenario: "join_idle, initial_chunks",
            evidence: EVIDENCE_UNIT,
        }),
        play::CLIENTBOUND_TILE_ENTITY_DATA_ID => {
            handled(state_only("update block entity in cached chunk snapshot"))
        }
        play::CLIENTBOUND_UNLOAD_CHUNK_ID => handled(state_only("drop chunk from world cache")),
        play::CLIENTBOUND_BLOCK_CHANGE_ID => handled(state_only("update one cached block state")),
        play::CLIENTBOUND_MULTI_BLOCK_CHANGE_ID => {
            handled(state_only("update cached block states in one section"))
        }
        play::CLIENTBOUND_UPDATE_LIGHT_ID => {
            handled(state_only("replace light data in cached chunk snapshot"))
        }
        play::CLIENTBOUND_PING_ID => handled(Obligation {
            responds_with: Some("pong"),
            timing: TimingClass::Strict,
            state_update: "none",
            status: ConformanceStatus::Partial,
            scenario: "join_idle",
            evidence: EVIDENCE_UNIT,
        }),
        play::CLIENTBOUND_DEATH_COMBAT_EVENT_ID => handled(state_only(
            "store structured local-player death information; emit HUD event",
        )),
        play::CLIENTBOUND_UPDATE_HEALTH_ID => {
            handled(state_only("update health/hunger/saturation"))
        }
        play::CLIENTBOUND_RESPAWN_ID => handled(state_only(
            "reset dimension/world and readiness gate for a new life",
        )),
        play::CLIENTBOUND_OPEN_WINDOW_ID => handled(state_only(
            "replace open container, cancel stale transactions and emit inventory events",
        )),
        play::CLIENTBOUND_CLOSE_WINDOW_ID => handled(state_only(
            "close matching container, cancel transactions and emit inventory events",
        )),
        play::CLIENTBOUND_WINDOW_ITEMS_ID => handled(state_only(
            "apply bounded full slot/cursor correction and resolve transactions",
        )),
        play::CLIENTBOUND_SET_SLOT_ID => handled(state_only(
            "apply bounded slot update and confirm newer-state transactions",
        )),
        play::CLIENTBOUND_SET_CURSOR_ITEM_ID => handled(state_only(
            "update authoritative cursor item and emit event",
        )),
        play::CLIENTBOUND_CRAFT_PROGRESS_BAR_ID => handled(state_only(
            "update bounded deterministic window property state and emit event",
        )),
        play::CLIENTBOUND_HELD_ITEM_SLOT_ID => {
            handled(state_only("validate and track the selected hotbar slot"))
        }
        play::CLIENTBOUND_SET_PLAYER_INVENTORY_ID => handled(state_only(
            "update bounded player inventory plus hotbar/held-item projections; emit events",
        )),
        play::CLIENTBOUND_PLAYER_INFO_ID => handled(state_only(
            "apply bounded deterministic player-list fields and emit join/HUD events",
        )),
        play::CLIENTBOUND_PLAYER_REMOVE_ID => handled(state_only(
            "remove deterministic player-list entries and emit leave/HUD events",
        )),
        play::CLIENTBOUND_UPDATE_TIME_ID => handled(state_only(
            "store world age, day time and ticking flag; emit time/HUD events",
        )),
        play::CLIENTBOUND_GAME_STATE_CHANGE_ID => handled(state_only(
            "store game mode and rain/thunder state; emit weather/HUD events",
        )),
        play::CLIENTBOUND_PLAYER_CHAT_ID => handled(state_only(
            "store safe display text plus unverified raw signed-chat data; emit event",
        )),
        play::CLIENTBOUND_SYSTEM_CHAT_ID => handled(state_only(
            "store system chat or action bar according to packet flag; emit event",
        )),
        play::CLIENTBOUND_PROFILELESS_CHAT_ID => handled(state_only(
            "store structured disguised chat metadata and emit event",
        )),
        play::CLIENTBOUND_ACTION_BAR_ID => handled(state_only(
            "replace structured action-bar state and emit event",
        )),
        play::CLIENTBOUND_SET_TITLE_TEXT_ID => {
            handled(state_only("replace structured title and emit event"))
        }
        play::CLIENTBOUND_SET_TITLE_SUBTITLE_ID => {
            handled(state_only("replace structured subtitle and emit event"))
        }
        play::CLIENTBOUND_SET_TITLE_TIME_ID => {
            handled(state_only("replace title timing values and emit event"))
        }
        play::CLIENTBOUND_CLEAR_TITLES_ID => handled(state_only(
            "clear title/subtitle; reset default timings only when requested",
        )),
        play::CLIENTBOUND_PLAYERLIST_HEADER_ID => handled(state_only(
            "replace structured tab-list header/footer and emit event",
        )),
        play::CLIENTBOUND_BOSS_BAR_ID => handled(state_only(
            "apply bounded add/update/remove state by stable uuid and emit event",
        )),
        play::CLIENTBOUND_RESET_SCORE_ID => handled(state_only(
            "remove one/all bounded scores for an owner and emit event",
        )),
        play::CLIENTBOUND_SCOREBOARD_DISPLAY_OBJECTIVE_ID => handled(state_only(
            "attach/detach bounded display slot by stable objective name and emit event",
        )),
        play::CLIENTBOUND_SCOREBOARD_OBJECTIVE_ID => handled(state_only(
            "create/update/remove bounded objective; detach slots/scores on removal; emit event",
        )),
        play::CLIENTBOUND_TEAMS_ID => handled(state_only(
            "apply bounded team lifecycle/options/membership by stable name and emit event",
        )),
        play::CLIENTBOUND_SCOREBOARD_SCORE_ID => handled(state_only(
            "create/update bounded score with display/number formatting and emit event",
        )),
        play::CLIENTBOUND_START_CONFIGURATION_ID => ignored(not_implemented(
            Some("configuration_acknowledged"),
            TimingClass::Strict,
            "re-enter configuration state",
            "",
        )),
        play::CLIENTBOUND_COOKIE_REQUEST_ID => unsupported(Some("cookie_response")),
        play::CLIENTBOUND_CUSTOM_PAYLOAD_ID => ignored(Obligation {
            state_update: "vanilla answers known plugin channels; brand sent voluntarily",
            status: ConformanceStatus::NotImplemented,
            evidence: EVIDENCE_NONE,
            ..NO_RESPONSE
        }),
        play::CLIENTBOUND_ADD_RESOURCE_PACK_ID => handled(Obligation {
            responds_with: Some("resource_pack_receive"),
            timing: TimingClass::Strict,
            state_update: "none",
            status: ConformanceStatus::Partial,
            scenario: "resource_pack",
            evidence: EVIDENCE_MOCK,
        }),
        play::CLIENTBOUND_REMOVE_RESOURCE_PACK_ID => {
            handled(state_only("no client-side pack state to remove"))
        }
        play::CLIENTBOUND_TRANSFER_ID => ignored(not_implemented(
            None,
            TimingClass::Strict,
            "reconnect to another server",
            "",
        )),
        play::CLIENTBOUND_STORE_COOKIE_ID => ignored(Obligation {
            status: ConformanceStatus::NotImplemented,
            ..NO_RESPONSE
        }),
        play::CLIENTBOUND_TAGS_ID => CoverageEntry {
            class: CoverageClass::StoredForLater,
            obligation: not_implemented(None, TimingClass::None, "store tags", ""),
        },
        _ => DEFAULT_ENTRY,
    }
}

fn class_name(class: CoverageClass) -> &'static str {
    match class {
        CoverageClass::Handled => "handled",
        CoverageClass::IntentionallyIgnored => "ignored",
        CoverageClass::StoredForLater => "stored",
        CoverageClass::Unsupported => "UNSUPPORTED",
    }
}

fn timing_name(timing: TimingClass) -> &'static str {
    match timing {
        TimingClass::Strict => "strict",
        TimingClass::TickBound => "tick-bound",
        TimingClass::Periodic => "periodic",
        TimingClass::BestEffort => "best-effort",
        TimingClass::None => "—",
    }
}

fn status_name(status: ConformanceStatus) -> &'static str {
    match status {
        ConformanceStatus::Pass => "PASS",
        ConformanceStatus::Partial => "PARTIAL",
        ConformanceStatus::Fail => "FAIL",
        ConformanceStatus::NotImplemented => "NOT IMPLEMENTED",
        ConformanceStatus::NotApplicable => "NOT APPLICABLE",
    }
}

/// Header line of the generated conformance document.
pub const MATRIX_HEADER: &str =
    "<!-- @generated by `cargo run --bin conformance_matrix`; DO NOT EDIT MANUALLY -->";

fn state_table(out: &mut String, title: &str, state: ConnectionState, ids: &[i32]) {
    out.push_str(&format!("## {title}\n\n"));
    out.push_str("| id | packet | class | responds with | timing | state update | status | scenario | evidence |\n");
    out.push_str("|---:|---|---|---|---|---|---|---|---|\n");
    for &id in ids {
        let name =
            crate::trace::decode::packet_name(state, crate::trace::Direction::Clientbound, id)
                .unwrap_or("UNKNOWN");
        let entry = clientbound_coverage(state, id);
        let responds = entry.obligation.responds_with.unwrap_or("—");
        out.push_str(&format!(
            "| {id} | {name} | {} | {responds} | {} | {} | {} | {} | {} |\n",
            class_name(entry.class),
            timing_name(entry.obligation.timing),
            entry.obligation.state_update,
            status_name(entry.obligation.status),
            entry.obligation.scenario,
            entry.obligation.evidence,
        ));
    }
    out.push('\n');
}

/// Renders `docs/vanilla_conformance_1_21_4.md` from the live coverage
/// table and the generated registries. Single source of truth for the
/// `conformance_matrix` binary and the drift test.
pub fn render_matrix() -> String {
    let mut out = String::new();
    out.push_str(MATRIX_HEADER);
    out.push_str("\n\n# Vanilla conformance matrix — Minecraft 1.21.4 (protocol 769)\n\n");
    out.push_str(
        "Client obligation matrix for every clientbound packet in `login`, `configuration`\n\
         and `play`. Generated from `src/minecraft/coverage.rs` against the generated\n\
         minecraft-data registries, so packet coverage cannot silently drift.\n\n\
         Statuses: `PASS` requires a real vanilla 1.21.4 capture plus a passing semantic\n\
         diff. Mock-server and golden tests only justify `PARTIAL`. No packet may be\n\
         absent from this table: the drift test fails if the generated registries gain\n\
         or lose an id.\n\n",
    );
    state_table(
        &mut out,
        "Login",
        ConnectionState::Login,
        login::CLIENTBOUND_IDS,
    );
    state_table(
        &mut out,
        "Configuration",
        ConnectionState::Configuration,
        configuration::CLIENTBOUND_IDS,
    );
    state_table(
        &mut out,
        "Play",
        ConnectionState::Play,
        play::CLIENTBOUND_IDS,
    );
    out
}
