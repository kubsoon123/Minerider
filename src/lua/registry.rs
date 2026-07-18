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

    fn allocate_id(&mut self) -> u32 {
        while self.bots.contains_key(&self.next_auto_id) {
            self.next_auto_id += 1;
        }
        let id = self.next_auto_id;
        self.next_auto_id += 1;
        id
    }

    pub fn add_bot(&mut self, spec: BotSpec) -> Result<u32, RegistryError> {
        if spec.username.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "bot username must not be empty".to_string(),
            ));
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
        let id = match spec.id {
            Some(id) => {
                if self.bots.contains_key(&id) {
                    return Err(RegistryError::DuplicateBotId(id));
                }
                id
            }
            None => self.allocate_id(),
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

    pub fn add_group(&mut self, name: String, bot_ids: Vec<u32>) -> Result<(), RegistryError> {
        if name.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "group name must not be empty".to_string(),
            ));
        }
        if self.groups.contains_key(&name) {
            return Err(RegistryError::DuplicateGroup(name));
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
}
