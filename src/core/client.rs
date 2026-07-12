//! High-level client facade: connect (handshake → login → configuration)
//! and run the play-state loop.

use tracing::info;

use minerider_protocol::generated::versions::{ProtocolVersion, V1_21_4};

use crate::core::error::Result;
use crate::core::state::ConnectionState;
use crate::minecraft::{configuration, handshake, login, play};
use crate::network::connection::Connection;
use crate::trace::TraceRecorder;

/// Parameters required to connect to a server.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Server hostname or IP address.
    pub host: String,
    /// Server port (vanilla default: 25565).
    pub port: u16,
    /// Offline-mode username.
    pub username: String,
    /// Protocol version to advertise in the handshake.
    pub version: ProtocolVersion,
}

impl ClientConfig {
    /// Creates a config for Minecraft 1.21.4 (protocol 769).
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            version: V1_21_4,
        }
    }
}

/// A connected Minecraft client in [`ConnectionState::Play`].
pub struct Client {
    conn: Connection,
    /// UUID assigned by the server during login.
    pub uuid: u128,
    /// Username confirmed by the server during login.
    pub username: String,
}

impl Client {
    /// Connects and logs in: TCP → handshake → login → configuration.
    /// Returns the client ready for the play state.
    pub async fn connect(cfg: &ClientConfig) -> Result<Client> {
        Self::connect_inner(cfg, None).await
    }

    /// Like [`Client::connect`], but records every packet in both
    /// directions to the given trace recorder (vanilla-fidelity captures).
    pub async fn connect_with_trace(cfg: &ClientConfig, trace: TraceRecorder) -> Result<Client> {
        Self::connect_inner(cfg, Some(trace)).await
    }

    async fn connect_inner(cfg: &ClientConfig, trace: Option<TraceRecorder>) -> Result<Client> {
        info!(host = %cfg.host, port = cfg.port, version = %cfg.version.minecraft, "connecting");
        let mut conn = Connection::connect(&cfg.host, cfg.port).await?;
        if let Some(trace) = trace {
            conn.set_trace(trace);
        }

        handshake::send(&mut conn, cfg.version.protocol, &cfg.host, cfg.port).await?;
        conn.set_state(ConnectionState::Login);
        info!("handshake sent, entering login state");

        let success = login::login(&mut conn, &cfg.username).await?;
        info!("entering configuration state");
        configuration::run_configuration(&mut conn).await?;
        info!("entering play state");

        Ok(Client {
            conn,
            uuid: success.uuid,
            username: success.username,
        })
    }

    /// Runs the play-state loop until the connection errors or the server
    /// disconnects us.
    pub async fn run(&mut self) -> Result<()> {
        play::run_play(&mut self.conn).await
    }

    /// Current protocol state of the underlying connection.
    pub fn state(&self) -> ConnectionState {
        self.conn.state()
    }

    /// Sets the scenario step recorded on subsequent trace events.
    pub fn set_trace_step(&mut self, step: u32) {
        if let Some(trace) = self.conn.trace_mut() {
            trace.set_step(step);
        }
    }
}
