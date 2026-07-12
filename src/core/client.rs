//! High-level client facade: connect (handshake → login → configuration)
//! and run the play-state loop.

use tracing::info;

use crate::core::error::Result;
use crate::core::state::ConnectionState;
use crate::minecraft::{configuration, handshake, login, play};
use crate::network::connection::Connection;

/// Protocol version of Minecraft Java Edition 1.21.4.
pub const PROTOCOL_VERSION_1_21_4: i32 = 769;

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
    pub protocol_version: i32,
}

impl ClientConfig {
    /// Creates a config for Minecraft 1.21.4 (protocol 769).
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            protocol_version: PROTOCOL_VERSION_1_21_4,
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
        info!(host = %cfg.host, port = cfg.port, "connecting");
        let mut conn = Connection::connect(&cfg.host, cfg.port).await?;

        let hs = handshake::Handshake {
            protocol_version: cfg.protocol_version,
            server_address: &cfg.host,
            server_port: cfg.port,
            next_state: 2, // login
        };
        handshake::send(&mut conn, &hs).await?;
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
}
