//! Rust-owned swarm configuration: servers, proxies, bots, and groups.
//!
//! Built exactly once, during the coordinator worker's `swarm:configure(fn)`
//! callback (see `crate::lua::runtime`), then frozen and shared with every
//! worker as plain data — never as Lua state. No `mlua` dependency here on
//! purpose: this module is pure Rust domain data, independently testable
//! without a Lua VM, and it is the single source of truth other workers
//! read from instead of re-running `configure` themselves.
//!
//! **Proxy trust model.** Proxy endpoints and credentials are never
//! Lua-constructible. The host (the CLI, or any other embedder calling
//! `crate::lua::runtime::run_swarm`) supplies a fixed
//! [`ProxyProfiles`] map — profile id → a complete, already-resolved
//! `Arc<Socks5ProxyConfig>` — *before* any script runs. A script may only
//! reference a profile by its id (`add_bot({proxy = "profile_id"})`); it
//! can never choose an arbitrary host/port, and it can never choose which
//! environment variable a credential is read from. This closes an
//! exfiltration path that existed when scripts could supply
//! `username_env`/`password_env` themselves: a sandboxed script could pick
//! *any* environment variable name (e.g. an unrelated secret already in
//! the process's environment) and *any* destination host, and Rust would
//! faithfully read that variable and send it there as SOCKS5 auth — all
//! without ever calling the sandboxed `os.getenv` (which was never
//! exposed, but was never the actual gap). See `docs/lua_wrapper.md
//! #proxy-grouping-and-credentials` for the full writeup and
//! `crate::lua::runtime::proxy_profiles_from_env` for the CLI's own
//! (env-var-only, never-argv) way of building this map.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::core::supervisor::ReconnectPolicy;
use crate::network::socks5::Socks5ProxyConfig;

/// Profile id → fully-resolved proxy config, supplied by the host and
/// never derived from Lua. See the module doc comment.
pub type ProxyProfiles = std::collections::HashMap<String, Arc<Socks5ProxyConfig>>;

/// Conservative explicit bounds on every Lua-controlled quantity that
/// would otherwise let a script drive an unbounded allocation (a
/// `Vec::with_capacity`/`HashMap`/`String` sized directly off a
/// script-supplied count or length). No single mandatory value was
/// specified beyond "choose conservative limits" — these are chosen
/// generously above any real swarm this wrapper is designed for (see
/// `docs/lua_runtime_benchmark.md`'s measured scale of hundreds of bots),
/// while still being small enough that hitting one is unambiguously a
/// misconfiguration, not a legitimate large deployment.
pub const MAX_BOTS: usize = 20_000;
pub const MAX_GROUPS: usize = 4_000;
pub const MAX_BOTS_PER_GROUP: usize = 20_000;
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_USERNAME_LEN: usize = 64;
pub const MAX_LABEL_LEN: usize = 256;

/// A named Minecraft server endpoint bots can be assigned to.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerDef {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub view_distance: i8,
    pub write_timeout: Duration,
    pub connect_deadline: Duration,
    /// Whether bots on this server participate in the process-wide shared
    /// chunk store. Identity for sharing purposes is derived from
    /// `host`/`port` only (see `crate::minecraft::shared_world::ServerIdentity`)
    /// — proxy assignment can never affect which bots share payloads.
    pub shared_chunks: bool,
    /// Whether bots accept and validate server resource packs. Mirrors the
    /// vanilla client option and defaults to enabled.
    pub accept_resource_packs: bool,
}

impl Default for ServerDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 25565,
            view_distance: crate::minecraft::DEFAULT_VIEW_DISTANCE,
            write_timeout: Duration::from_secs(10),
            connect_deadline: Duration::from_secs(15),
            shared_chunks: true,
            accept_resource_packs: true,
        }
    }
}

/// Raised when a bot/group references a proxy profile id the host never
/// registered. Deliberately carries only the id the script asked for —
/// never any host/port/credential detail, so this error is always safe to
/// log or return to Lua verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("proxy profile `{0}` is not configured")]
pub struct UnknownProxyProfile(pub String);

/// One bot's static configuration. `id` is the stable numeric identity used
/// for deterministic worker assignment (`id % worker_count`) — it never
/// changes across reconnect, username change, or proxy reassignment,
/// because it is never reassigned at all once the registry is built.
#[derive(Debug, Clone, PartialEq)]
pub struct BotDef {
    pub id: u32,
    pub username: String,
    pub server: String,
    pub proxy: Option<String>,
    pub reconnect: ReconnectPolicy,
    /// Optional human-readable label (e.g. from `add_group`'s `id_prefix`)
    /// for logs/diagnostics only — never used for worker assignment or
    /// identity comparisons.
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupDef {
    pub name: String,
    pub bot_ids: Vec<u32>,
}

/// Input to [`SwarmRegistryBuilder::add_bot`].
#[derive(Debug, Clone, PartialEq)]
pub struct BotSpec {
    pub id: Option<u32>,
    pub username: String,
    pub server: String,
    pub proxy: Option<String>,
    pub reconnect: ReconnectPolicy,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("server `{0}` is already defined")]
    DuplicateServer(String),
    #[error("bot id {0} is already in use")]
    DuplicateBotId(u32),
    #[error("bot username `{0}` is already in use")]
    DuplicateUsername(String),
    #[error("group `{0}` is already defined")]
    DuplicateGroup(String),
    #[error("server `{0}` is not defined")]
    UnknownServer(String),
    /// The script referenced a proxy profile id the host never registered
    /// in `crate::lua::registry::ProxyProfiles`. Never carries host/port/
    /// credential detail — see [`UnknownProxyProfile`].
    #[error(transparent)]
    UnknownProxy(#[from] UnknownProxyProfile),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}

impl RegistryError {
    /// Stable error code for the Lua-facing `{code, message, ...}` error
    /// table (see `crate::lua::api::errors`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateServer(_)
            | Self::DuplicateBotId(_)
            | Self::DuplicateUsername(_)
            | Self::DuplicateGroup(_) => "duplicate_id",
            Self::UnknownServer(_) => "unknown_server",
            Self::UnknownProxy(_) => "unknown_proxy",
            Self::InvalidConfiguration(_) => "invalid_configuration",
        }
    }
}

/// Accumulates `add_server`/`add_bot`/`add_group` calls made during the
/// coordinator's `swarm:configure(fn)` callback, validating each one
/// immediately so a misconfiguration fails fast with a precise error
/// rather than surfacing later as a confusing connect-time failure.
///
/// There is deliberately no `add_proxy`: the set of valid proxy profile
/// ids (`proxy_profile_ids`) is supplied by the host at construction time
/// (mirroring `crate::lua::registry::ProxyProfiles`, but holding only ids
/// — never the actual `Socks5ProxyConfig`/credentials, which this builder
/// has no need to see) and is read-only for the whole configuration
/// phase. A script can reference a profile by id; it can never define,
/// rename, or replace one. See the module doc comment.
#[derive(Debug)]
pub struct SwarmRegistryBuilder {
    servers: BTreeMap<String, ServerDef>,
    proxy_profile_ids: Arc<BTreeSet<String>>,
    bots: BTreeMap<u32, BotDef>,
    username_to_id: BTreeMap<String, u32>,
    groups: BTreeMap<String, GroupDef>,
    next_auto_id: u32,
}

impl Default for SwarmRegistryBuilder {
    fn default() -> Self {
        Self::new(Arc::new(BTreeSet::new()))
    }
}

impl SwarmRegistryBuilder {
    /// `proxy_profile_ids` is the set of profile ids the host registered
    /// for this run (see `crate::lua::runtime::SwarmRuntimeConfig::proxy_profiles`)
    /// — the only proxy references a script will ever be allowed to make.
    pub fn new(proxy_profile_ids: Arc<BTreeSet<String>>) -> Self {
        Self {
            servers: BTreeMap::new(),
            proxy_profile_ids,
            bots: BTreeMap::new(),
            username_to_id: BTreeMap::new(),
            groups: BTreeMap::new(),
            next_auto_id: 0,
        }
    }

    pub fn add_server(&mut self, def: ServerDef) -> Result<(), RegistryError> {
        if def.name.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "server name must not be empty".to_string(),
            ));
        }
        if def.name.len() > MAX_NAME_LEN {
            return Err(RegistryError::InvalidConfiguration(format!(
                "server name exceeds the {MAX_NAME_LEN}-byte limit"
            )));
        }
        if def.host.is_empty() {
            return Err(RegistryError::InvalidConfiguration(format!(
                "server `{}` has an empty host",
                def.name
            )));
        }
        if !(2..=32).contains(&def.view_distance) {
            return Err(RegistryError::InvalidConfiguration(format!(
                "server `{}` has view_distance {} outside 2..=32",
                def.name, def.view_distance
            )));
        }
        if self.servers.contains_key(&def.name) {
            return Err(RegistryError::DuplicateServer(def.name));
        }
        self.servers.insert(def.name.clone(), def);
        Ok(())
    }

    /// Finds the next free id starting from `next_auto_id`, bounded so a
    /// swarm with bot ids clustered near `u32::MAX` can never spin
    /// forever (or overflow the counter) searching for a free slot: once
    /// every remaining id up to `u32::MAX` is exhausted, this reports
    /// failure instead of wrapping back to `0` and silently colliding
    /// with an already-allocated low id.
    fn allocate_id(&mut self) -> Result<u32, RegistryError> {
        loop {
            if !self.bots.contains_key(&self.next_auto_id) {
                let id = self.next_auto_id;
                match self.next_auto_id.checked_add(1) {
                    Some(next) => self.next_auto_id = next,
                    None => self.next_auto_id = u32::MAX, // saturate; next call re-checks id u32::MAX itself
                }
                return Ok(id);
            }
            match self.next_auto_id.checked_add(1) {
                Some(next) => self.next_auto_id = next,
                None => {
                    return Err(RegistryError::InvalidConfiguration(
                        "no free auto-assigned bot id remains up to u32::MAX".to_string(),
                    ))
                }
            }
        }
    }

    pub fn add_bot(&mut self, spec: BotSpec) -> Result<u32, RegistryError> {
        if spec.username.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "bot username must not be empty".to_string(),
            ));
        }
        if spec.username.len() > MAX_USERNAME_LEN {
            return Err(RegistryError::InvalidConfiguration(format!(
                "bot username exceeds the {MAX_USERNAME_LEN}-byte limit"
            )));
        }
        if let Some(label) = &spec.label {
            if label.len() > MAX_LABEL_LEN {
                return Err(RegistryError::InvalidConfiguration(format!(
                    "bot label exceeds the {MAX_LABEL_LEN}-byte limit"
                )));
            }
        }
        if !self.servers.contains_key(&spec.server) {
            return Err(RegistryError::UnknownServer(spec.server));
        }
        if let Some(proxy) = &spec.proxy {
            if !self.proxy_profile_ids.contains(proxy) {
                return Err(UnknownProxyProfile(proxy.clone()).into());
            }
        }
        if self.username_to_id.contains_key(&spec.username) {
            return Err(RegistryError::DuplicateUsername(spec.username));
        }
        if self.bots.len() >= MAX_BOTS {
            return Err(RegistryError::InvalidConfiguration(format!(
                "swarm already has the maximum of {MAX_BOTS} bots"
            )));
        }
        let id = match spec.id {
            Some(id) => {
                if self.bots.contains_key(&id) {
                    return Err(RegistryError::DuplicateBotId(id));
                }
                id
            }
            None => self.allocate_id()?,
        };
        self.next_auto_id = self.next_auto_id.max(id.saturating_add(1));
        self.username_to_id.insert(spec.username.clone(), id);
        self.bots.insert(
            id,
            BotDef {
                id,
                username: spec.username,
                server: spec.server,
                proxy: spec.proxy,
                reconnect: spec.reconnect,
                label: spec.label,
            },
        );
        Ok(id)
    }

    /// Reverses a successful [`add_bot`](Self::add_bot) call — the sole
    /// purpose is letting a caller that adds several bots as one logical,
    /// all-or-nothing batch (e.g. `swarm:add_group`'s bulk bot creation)
    /// roll back everything it already added the moment any one of them
    /// fails, rather than leaving a partial, ungrouped batch behind. Does
    /// *not* rewind `next_auto_id` — a removed id is simply never reused
    /// within this same builder, which is harmless (ids only need to be
    /// unique, not gap-free) and far simpler than trying to safely
    /// "undo" auto-assignment ordering.
    pub fn remove_bot(&mut self, id: u32) -> bool {
        match self.bots.remove(&id) {
            Some(def) => {
                self.username_to_id.remove(&def.username);
                true
            }
            None => false,
        }
    }

    pub fn add_group(&mut self, name: String, bot_ids: Vec<u32>) -> Result<(), RegistryError> {
        if name.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "group name must not be empty".to_string(),
            ));
        }
        if name.len() > MAX_NAME_LEN {
            return Err(RegistryError::InvalidConfiguration(format!(
                "group name exceeds the {MAX_NAME_LEN}-byte limit"
            )));
        }
        if bot_ids.len() > MAX_BOTS_PER_GROUP {
            return Err(RegistryError::InvalidConfiguration(format!(
                "group `{name}` has {} bots, exceeding the {MAX_BOTS_PER_GROUP} limit",
                bot_ids.len()
            )));
        }
        if self.groups.contains_key(&name) {
            return Err(RegistryError::DuplicateGroup(name));
        }
        if self.groups.len() >= MAX_GROUPS {
            return Err(RegistryError::InvalidConfiguration(format!(
                "swarm already has the maximum of {MAX_GROUPS} groups"
            )));
        }
        for id in &bot_ids {
            if !self.bots.contains_key(id) {
                return Err(RegistryError::InvalidConfiguration(format!(
                    "group `{name}` references unknown bot id {id}"
                )));
            }
        }
        self.groups.insert(name.clone(), GroupDef { name, bot_ids });
        Ok(())
    }

    pub fn build(self) -> SwarmRegistry {
        SwarmRegistry {
            servers: self.servers,
            bots: self.bots,
            username_to_id: self.username_to_id,
            groups: self.groups,
        }
    }
}

/// The frozen, validated configuration produced by exactly one
/// `swarm:configure(fn)` run on the coordinator worker. Immutable and
/// `Clone`-cheap-ish (an `Arc<SwarmRegistry>` is what actually gets shared);
/// every worker reads the same data instead of re-deriving it.
///
/// Deliberately has no `proxies` field: the actual proxy configs
/// (including credentials) live only in the host-owned
/// `crate::lua::registry::ProxyProfiles` map passed to
/// `crate::lua::runtime::run_swarm`, resolved directly by profile id at
/// bot-spawn time — never staged through the registry, which only ever
/// needs to know a `BotDef.proxy` *id string* was validated against that
/// map's keys at `add_bot` time.
#[derive(Debug, Clone, Default)]
pub struct SwarmRegistry {
    pub servers: BTreeMap<String, ServerDef>,
    pub bots: BTreeMap<u32, BotDef>,
    pub username_to_id: BTreeMap<String, u32>,
    pub groups: BTreeMap<String, GroupDef>,
}

impl SwarmRegistry {
    /// Deterministic worker assignment: never changes across a bot's
    /// lifetime, reconnect, username change, or proxy assignment, because
    /// it is a pure function of the bot's immutable numeric id.
    pub fn worker_index(&self, bot_id: u32, worker_count: usize) -> usize {
        debug_assert!(worker_count > 0);
        (bot_id as usize) % worker_count
    }

    pub fn bot_by_username(&self, username: &str) -> Option<&BotDef> {
        self.username_to_id
            .get(username)
            .and_then(|id| self.bots.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerDef {
        ServerDef {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: 25565,
            ..ServerDef::default()
        }
    }

    fn bot(username: &str, server: &str) -> BotSpec {
        BotSpec {
            id: None,
            username: username.to_string(),
            server: server.to_string(),
            proxy: None,
            reconnect: ReconnectPolicy::default(),
            label: None,
        }
    }

    fn builder_with_profiles(ids: &[&str]) -> SwarmRegistryBuilder {
        SwarmRegistryBuilder::new(Arc::new(ids.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn auto_assigned_bot_ids_are_sequential_and_unique() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let id0 = b.add_bot(bot("alice", "main")).unwrap();
        let id1 = b.add_bot(bot("bob", "main")).unwrap();
        let id2 = b.add_bot(bot("carol", "main")).unwrap();
        assert_eq!([id0, id1, id2], [0, 1, 2]);
    }

    #[test]
    fn explicit_bot_id_is_respected_and_auto_assignment_skips_past_it() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.id = Some(5);
        assert_eq!(b.add_bot(spec).unwrap(), 5);
        // Auto-assignment must not collide with the explicit id above.
        let next = b.add_bot(bot("bob", "main")).unwrap();
        assert_ne!(next, 5);
    }

    #[test]
    fn duplicate_explicit_bot_id_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec_a = bot("alice", "main");
        spec_a.id = Some(1);
        b.add_bot(spec_a).unwrap();
        let mut spec_b = bot("bob", "main");
        spec_b.id = Some(1);
        let err = b.add_bot(spec_b).unwrap_err();
        assert_eq!(err.code(), "duplicate_id");
        assert!(matches!(err, RegistryError::DuplicateBotId(1)));
    }

    #[test]
    fn duplicate_username_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        b.add_bot(bot("alice", "main")).unwrap();
        let err = b.add_bot(bot("alice", "main")).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateUsername(_)));
    }

    #[test]
    fn bot_referencing_unknown_server_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        let err = b.add_bot(bot("alice", "ghost")).unwrap_err();
        assert_eq!(err.code(), "unknown_server");
    }

    #[test]
    fn bot_referencing_a_proxy_profile_the_host_never_registered_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.proxy = Some("ghost-profile".to_string());
        let err = b.add_bot(spec).unwrap_err();
        assert_eq!(err.code(), "unknown_proxy");
        // The error must carry only the id the script asked for, never
        // credential/host/port detail (there is none to carry — the
        // builder itself never sees `Socks5ProxyConfig` at all).
        assert_eq!(
            err.to_string(),
            "proxy profile `ghost-profile` is not configured"
        );
    }

    #[test]
    fn bot_referencing_a_host_registered_proxy_profile_succeeds() {
        let mut b = builder_with_profiles(&["proxy1", "proxy2"]);
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.proxy = Some("proxy1".to_string());
        assert!(b.add_bot(spec).is_ok());
    }

    #[test]
    fn a_script_cannot_widen_the_proxy_profile_set_there_is_no_add_proxy_method() {
        // Compile-time proof, not a runtime assertion: `SwarmRegistryBuilder`
        // has no `add_proxy` method at all (see the struct's doc comment) —
        // the only way profile ids become valid is via the `Arc<BTreeSet<String>>`
        // passed into `new`, which only the host (never Lua) constructs.
        let b = builder_with_profiles(&["proxy1"]);
        assert_eq!(b.proxy_profile_ids.len(), 1);
        assert!(b.proxy_profile_ids.contains("proxy1"));
    }

    #[test]
    fn duplicate_server_name_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let err = b.add_server(server("main")).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateServer(_)));
    }

    #[test]
    fn group_referencing_unknown_bot_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let id = b.add_bot(bot("alice", "main")).unwrap();
        b.add_group("g1".to_string(), vec![id]).unwrap();
        let err = b.add_group("g2".to_string(), vec![999]).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn worker_index_is_a_pure_function_of_bot_id() {
        let registry = SwarmRegistry::default();
        assert_eq!(registry.worker_index(0, 4), 0);
        assert_eq!(registry.worker_index(1, 4), 1);
        assert_eq!(registry.worker_index(4, 4), 0);
        assert_eq!(registry.worker_index(7, 4), 3);
    }

    #[test]
    fn server_name_over_the_length_limit_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        let mut def = server("main");
        def.name = "x".repeat(MAX_NAME_LEN + 1);
        let err = b.add_server(def).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn bot_username_over_the_length_limit_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec = bot(&"x".repeat(MAX_USERNAME_LEN + 1), "main");
        spec.username = "x".repeat(MAX_USERNAME_LEN + 1);
        let err = b.add_bot(spec).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn bot_label_over_the_length_limit_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.label = Some("x".repeat(MAX_LABEL_LEN + 1));
        let err = b.add_bot(spec).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn group_name_over_the_length_limit_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let id = b.add_bot(bot("alice", "main")).unwrap();
        let err = b
            .add_group("g".repeat(MAX_NAME_LEN + 1), vec![id])
            .unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn adding_more_than_max_bots_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        for i in 0..MAX_BOTS {
            let mut spec = bot(&format!("bot{i}"), "main");
            spec.id = Some(i as u32);
            b.add_bot(spec).unwrap();
        }
        let mut one_more = bot("one_too_many", "main");
        one_more.id = Some(MAX_BOTS as u32);
        let err = b.add_bot(one_more).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
        assert_eq!(b.bots.len(), MAX_BOTS);
    }

    #[test]
    fn adding_more_than_max_groups_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let id = b.add_bot(bot("alice", "main")).unwrap();
        for i in 0..MAX_GROUPS {
            b.add_group(format!("g{i}"), vec![id]).unwrap();
        }
        let err = b
            .add_group("one_too_many".to_string(), vec![id])
            .unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
        assert_eq!(b.groups.len(), MAX_GROUPS);
    }

    #[test]
    fn a_group_with_more_bots_than_the_per_group_limit_is_rejected() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        // Duplicate ids are fine for this check — it must reject on
        // length alone, before ever validating individual bot ids.
        let bot_ids = vec![0u32; MAX_BOTS_PER_GROUP + 1];
        let err = b.add_group("too_big".to_string(), bot_ids).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn remove_bot_reverses_a_successful_add_bot_including_the_username_reservation() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let id = b.add_bot(bot("alice", "main")).unwrap();
        assert!(b.remove_bot(id));
        assert!(!b.bots.contains_key(&id));
        // The username must be free again — re-adding it must succeed.
        assert!(b.add_bot(bot("alice", "main")).is_ok());
    }

    #[test]
    fn remove_bot_on_an_unknown_id_is_a_harmless_no_op() {
        let mut b = SwarmRegistryBuilder::default();
        assert!(!b.remove_bot(999));
    }

    #[test]
    fn auto_id_allocation_reports_a_typed_error_instead_of_overflowing_past_u32_max() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.id = Some(u32::MAX);
        b.add_bot(spec).unwrap();
        // `next_auto_id` is now pinned at `u32::MAX` (the explicit id
        // above pushed it there), and that exact id is already taken —
        // the next auto-assignment must return a typed error (this
        // builder never searches backward for a lower gap once pinned at
        // the top) rather than panicking on `u32::MAX + 1` overflow or
        // looping forever. This call returning *at all*, promptly, is
        // itself the regression proof.
        let err = b.add_bot(bot("bob", "main")).unwrap_err();
        assert_eq!(err.code(), "invalid_configuration");
    }

    #[test]
    fn auto_id_allocation_still_works_normally_after_an_unrelated_high_explicit_id() {
        let mut b = SwarmRegistryBuilder::default();
        b.add_server(server("main")).unwrap();
        // An explicit id (already-existing behavior, unchanged by this
        // fix) advances `next_auto_id` past it — proving that ordinary
        // path still just works (returns *some* fresh, non-conflicting
        // id) after this fix's changes, not specifically that it reuses
        // any particular gap.
        let mut spec = bot("alice", "main");
        spec.id = Some(1000);
        b.add_bot(spec).unwrap();
        let auto_id = b.add_bot(bot("bob", "main")).unwrap();
        assert_ne!(auto_id, 1000);
    }
}
