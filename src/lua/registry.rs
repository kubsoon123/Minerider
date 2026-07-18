//! Rust-owned swarm configuration: servers, proxies, bots, and groups.
//!
//! Built exactly once, during the coordinator worker's `swarm:configure(fn)`
//! callback (see `crate::lua::runtime`), then frozen and shared with every
//! worker as plain data — never as Lua state. No `mlua` dependency here on
//! purpose: this module is pure Rust domain data, independently testable
//! without a Lua VM, and it is the single source of truth other workers
//! read from instead of re-running `configure` themselves.
//!
//! Proxy credentials are the one deliberately asymmetric piece: Lua only
//! ever supplies environment variable *names* (see [`ProxyDef`]); the
//! actual secret values are resolved by [`ProxyDef::resolve`], which is
//! called from `crate::lua::runtime` outside any Lua context, and the
//! resulting `Socks5ProxyConfig` (with real credentials inside) is never
//! handed back to Lua.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::core::supervisor::ReconnectPolicy;
use crate::network::socks5::{Socks5Credentials, Socks5ProxyConfig};

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

/// A named SOCKS5 proxy handle. Many bots may reference one by name.
/// Credentials are never stored as plaintext here — only the names of the
/// environment variables that hold them.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyDef {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username_env: Option<String>,
    pub password_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyResolveError {
    #[error("proxy `{proxy}` references environment variable `{var}`, which is not set")]
    MissingEnvVar { proxy: String, var: String },
}

impl ProxyDef {
    /// Resolves this proxy's credentials from the environment. Called only
    /// from `crate::lua::runtime`, outside any Lua context — Lua itself
    /// never has a code path that can read an environment variable's
    /// value, only specify its name (see `crate::lua::sandbox`, which never
    /// exposes `os.getenv`).
    pub fn resolve(&self) -> Result<Arc<Socks5ProxyConfig>, ProxyResolveError> {
        let mut cfg = Socks5ProxyConfig::new(self.host.clone(), self.port);
        match (&self.username_env, &self.password_env) {
            (None, None) => {}
            (Some(user_var), Some(pass_var)) => {
                let username =
                    std::env::var(user_var).map_err(|_| ProxyResolveError::MissingEnvVar {
                        proxy: self.name.clone(),
                        var: user_var.clone(),
                    })?;
                let password =
                    std::env::var(pass_var).map_err(|_| ProxyResolveError::MissingEnvVar {
                        proxy: self.name.clone(),
                        var: pass_var.clone(),
                    })?;
                cfg = cfg.with_credentials(Socks5Credentials::new(username, password));
            }
            (Some(var), None) | (None, Some(var)) => {
                return Err(ProxyResolveError::MissingEnvVar {
                    proxy: self.name.clone(),
                    var: format!(
                        "{var} (both username_env and password_env are required together)"
                    ),
                });
            }
        }
        Ok(Arc::new(cfg))
    }
}

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
    #[error("proxy `{0}` is already defined")]
    DuplicateProxy(String),
    #[error("bot id {0} is already in use")]
    DuplicateBotId(u32),
    #[error("bot username `{0}` is already in use")]
    DuplicateUsername(String),
    #[error("group `{0}` is already defined")]
    DuplicateGroup(String),
    #[error("server `{0}` is not defined")]
    UnknownServer(String),
    #[error("proxy `{0}` is not defined")]
    UnknownProxy(String),
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
}

impl RegistryError {
    /// Stable error code for the Lua-facing `{code, message, ...}` error
    /// table (see `crate::lua::api::errors`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateServer(_)
            | Self::DuplicateProxy(_)
            | Self::DuplicateBotId(_)
            | Self::DuplicateUsername(_)
            | Self::DuplicateGroup(_) => "duplicate_id",
            Self::UnknownServer(_) => "unknown_server",
            Self::UnknownProxy(_) => "unknown_proxy",
            Self::InvalidConfiguration(_) => "invalid_configuration",
        }
    }
}

/// Accumulates `add_server`/`add_proxy`/`add_bot`/`add_group` calls made
/// during the coordinator's `swarm:configure(fn)` callback, validating each
/// one immediately so a misconfiguration fails fast with a precise error
/// rather than surfacing later as a confusing connect-time failure.
#[derive(Debug, Default)]
pub struct SwarmRegistryBuilder {
    servers: BTreeMap<String, ServerDef>,
    proxies: BTreeMap<String, ProxyDef>,
    bots: BTreeMap<u32, BotDef>,
    username_to_id: BTreeMap<String, u32>,
    groups: BTreeMap<String, GroupDef>,
    next_auto_id: u32,
}

impl SwarmRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
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

    pub fn add_proxy(&mut self, def: ProxyDef) -> Result<(), RegistryError> {
        if def.name.is_empty() {
            return Err(RegistryError::InvalidConfiguration(
                "proxy name must not be empty".to_string(),
            ));
        }
        if def.host.is_empty() {
            return Err(RegistryError::InvalidConfiguration(format!(
                "proxy `{}` has an empty host",
                def.name
            )));
        }
        if self.proxies.contains_key(&def.name) {
            return Err(RegistryError::DuplicateProxy(def.name));
        }
        self.proxies.insert(def.name.clone(), def);
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
            if !self.proxies.contains_key(proxy) {
                return Err(RegistryError::UnknownProxy(proxy.clone()));
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
            proxies: self.proxies,
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
#[derive(Debug, Clone, Default)]
pub struct SwarmRegistry {
    pub servers: BTreeMap<String, ServerDef>,
    pub proxies: BTreeMap<String, ProxyDef>,
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

    #[test]
    fn auto_assigned_bot_ids_are_sequential_and_unique() {
        let mut b = SwarmRegistryBuilder::new();
        b.add_server(server("main")).unwrap();
        let id0 = b.add_bot(bot("alice", "main")).unwrap();
        let id1 = b.add_bot(bot("bob", "main")).unwrap();
        let id2 = b.add_bot(bot("carol", "main")).unwrap();
        assert_eq!([id0, id1, id2], [0, 1, 2]);
    }

    #[test]
    fn explicit_bot_id_is_respected_and_auto_assignment_skips_past_it() {
        let mut b = SwarmRegistryBuilder::new();
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
        let mut b = SwarmRegistryBuilder::new();
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
        let mut b = SwarmRegistryBuilder::new();
        b.add_server(server("main")).unwrap();
        b.add_bot(bot("alice", "main")).unwrap();
        let err = b.add_bot(bot("alice", "main")).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateUsername(_)));
    }

    #[test]
    fn bot_referencing_unknown_server_is_rejected() {
        let mut b = SwarmRegistryBuilder::new();
        let err = b.add_bot(bot("alice", "ghost")).unwrap_err();
        assert_eq!(err.code(), "unknown_server");
    }

    #[test]
    fn bot_referencing_unknown_proxy_is_rejected() {
        let mut b = SwarmRegistryBuilder::new();
        b.add_server(server("main")).unwrap();
        let mut spec = bot("alice", "main");
        spec.proxy = Some("ghost-proxy".to_string());
        let err = b.add_bot(spec).unwrap_err();
        assert_eq!(err.code(), "unknown_proxy");
    }

    #[test]
    fn duplicate_server_name_is_rejected() {
        let mut b = SwarmRegistryBuilder::new();
        b.add_server(server("main")).unwrap();
        let err = b.add_server(server("main")).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateServer(_)));
    }

    #[test]
    fn group_referencing_unknown_bot_is_rejected() {
        let mut b = SwarmRegistryBuilder::new();
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
    fn proxy_resolve_requires_both_env_vars_or_neither() {
        let proxy = ProxyDef {
            name: "p1".to_string(),
            host: "127.0.0.1".to_string(),
            port: 1080,
            username_env: Some("MINERIDER_TEST_PROXY_USER_ONLY".to_string()),
            password_env: None,
        };
        let err = proxy.resolve().unwrap_err();
        assert!(matches!(err, ProxyResolveError::MissingEnvVar { .. }));
    }

    #[test]
    fn proxy_resolve_with_no_credentials_succeeds() {
        let proxy = ProxyDef {
            name: "p1".to_string(),
            host: "127.0.0.1".to_string(),
            port: 1080,
            username_env: None,
            password_env: None,
        };
        let cfg = proxy.resolve().unwrap();
        assert!(cfg.credentials.is_none());
    }

    #[test]
    fn proxy_resolve_reads_credentials_from_named_env_vars() {
        // SAFETY: test-only, single-threaded within this test's scope for
        // these specific unique var names.
        unsafe {
            std::env::set_var("MINERIDER_TEST_PROXY_USER_RESOLVE", "alice");
            std::env::set_var("MINERIDER_TEST_PROXY_PASS_RESOLVE", "hunter2");
        }
        let proxy = ProxyDef {
            name: "p1".to_string(),
            host: "127.0.0.1".to_string(),
            port: 1080,
            username_env: Some("MINERIDER_TEST_PROXY_USER_RESOLVE".to_string()),
            password_env: Some("MINERIDER_TEST_PROXY_PASS_RESOLVE".to_string()),
        };
        let cfg = proxy.resolve().unwrap();
        assert!(cfg.credentials.is_some());
        assert_eq!(cfg.credentials.as_ref().unwrap().username, "alice");
        unsafe {
            std::env::remove_var("MINERIDER_TEST_PROXY_USER_RESOLVE");
            std::env::remove_var("MINERIDER_TEST_PROXY_PASS_RESOLVE");
        }
    }
}
