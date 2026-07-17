//! High-level client facade: connect (handshake → login → configuration)
//! and run the play-state loop.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tracing::info;

use minerider_protocol::generated::versions::{ProtocolVersion, V1_21_4};

use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

use crate::auth::PremiumSession;
use crate::core::error::{MineRiderError, Result};
use crate::core::state::ConnectionState;
use crate::minecraft::configuration::ConfigurationData;
use crate::minecraft::control::{channel, BotCommand, ControlHandle};
use crate::minecraft::event::{BotEvent, EVENT_CHANNEL_CAPACITY};
use crate::minecraft::play::StateSnapshot;
use crate::minecraft::{configuration, handshake, login, play};
use crate::network::connection::{Connection, ConnectionTimeouts};
use crate::trace::TraceRecorder;

/// Default bound on a complete packet send (`write_all` + `flush` together);
/// see [`crate::network::connection`].
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default overall budget for TCP connect → handshake → login →
/// configuration (until the connection reaches Play state). A hostile or
/// broken server trickling one byte just before each per-read timeout would
/// otherwise stall a client indefinitely; this bounds the whole sequence
/// with one shared deadline instead of resetting it at every stage.
pub const DEFAULT_CONNECT_DEADLINE: Duration = Duration::from_secs(60);

/// Which high-level step of [`Client::connect`] was in flight when the
/// overall [`ClientConfig::connect_deadline`] expired, so the resulting
/// error says something more useful than "it timed out somewhere".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ConnectStage {
    TcpConnect = 0,
    Handshake = 1,
    Login = 2,
    Configuration = 3,
}

impl ConnectStage {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::TcpConnect,
            1 => Self::Handshake,
            2 => Self::Login,
            _ => Self::Configuration,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::TcpConnect => "TCP connect",
            Self::Handshake => "handshake",
            Self::Login => "login",
            Self::Configuration => "configuration",
        }
    }
}

/// Parameters required to connect to a server.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Server hostname or IP address.
    pub host: String,
    /// Server port (vanilla default: 25565).
    pub port: u16,
    /// Offline-mode username; ignored once [`premium`](Self::premium) is set,
    /// since the real profile's username is used instead.
    pub username: String,
    /// Protocol version to advertise in the handshake.
    pub version: ProtocolVersion,
    /// A completed Microsoft/Xbox Live sign-in (see [`crate::auth`]) to join
    /// online-mode servers as a real account. `None` connects offline-mode.
    pub premium: Option<PremiumSession>,
    /// The render distance sent in `client_information`. A real, ordinary
    /// client setting — lower values mean the server streams (and this
    /// client stores) fewer chunks, the main per-bot memory lever when
    /// running many bots on one machine.
    pub view_distance: i8,
    /// Bound on a single complete packet send (`write_all` + `flush`
    /// together). Defense in depth against a peer that stops reading and
    /// parks a write forever; see [`crate::network::connection`].
    pub write_timeout: Duration,
    /// Overall budget for [`Client::connect`]: TCP connect → handshake →
    /// login → configuration, as one shared deadline rather than one reset
    /// at every stage. The per-read timeout inside each stage still applies
    /// as defense in depth underneath this.
    pub connect_deadline: Duration,
}

impl ClientConfig {
    /// Creates an offline-mode config for Minecraft 1.21.4 (protocol 769),
    /// with vanilla's out-of-box render distance
    /// ([`crate::minecraft::DEFAULT_VIEW_DISTANCE`]).
    pub fn new(host: impl Into<String>, port: u16, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            version: V1_21_4,
            premium: None,
            view_distance: crate::minecraft::DEFAULT_VIEW_DISTANCE,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            connect_deadline: DEFAULT_CONNECT_DEADLINE,
        }
    }

    /// Attaches a premium session, so [`Client::connect`] identifies with
    /// the real account and joins the session server on online-mode servers.
    pub fn with_premium(mut self, session: PremiumSession) -> Self {
        self.premium = Some(session);
        self
    }

    /// Overrides the render distance sent in `client_information`.
    pub fn with_view_distance(mut self, view_distance: i8) -> Self {
        self.view_distance = view_distance;
        self
    }

    /// Overrides the packet-send timeout (see [`Self::write_timeout`]).
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Overrides the overall connect-to-play deadline (see
    /// [`Self::connect_deadline`]).
    pub fn with_connect_deadline(mut self, deadline: Duration) -> Self {
        self.connect_deadline = deadline;
        self
    }
}

/// A connected Minecraft client in [`ConnectionState::Play`].
pub struct Client {
    conn: Connection,
    /// UUID assigned by the server during login.
    pub uuid: u128,
    /// Username confirmed by the server during login.
    pub username: String,
    configuration: ConfigurationData,
    control_tx: ControlHandle,
    control_rx: Option<UnboundedReceiver<BotCommand>>,
    state_tx: Option<watch::Sender<StateSnapshot>>,
    state_rx: watch::Receiver<StateSnapshot>,
    /// Broadcast sender for [`BotEvent`]s; cloned into the play loop on each
    /// [`run`](Self::run) call, kept here so [`events`](Self::events) can
    /// hand out new subscriptions (including before `run` has ever started).
    event_tx: broadcast::Sender<BotEvent>,
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

    /// Runs the full connect sequence under one overall deadline (see
    /// [`ClientConfig::connect_deadline`]). Not retried automatically —
    /// callers wanting reconnection use [`crate::core::supervisor`].
    async fn connect_inner(cfg: &ClientConfig, trace: Option<TraceRecorder>) -> Result<Client> {
        let stage = AtomicU8::new(ConnectStage::TcpConnect as u8);
        match tokio::time::timeout(
            cfg.connect_deadline,
            Self::connect_stages(cfg, trace, &stage),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let reached = ConnectStage::from_u8(stage.load(Ordering::Relaxed));
                Err(MineRiderError::Timeout(format!(
                    "connect-to-play deadline ({:?}) exceeded during {}",
                    cfg.connect_deadline,
                    reached.label()
                )))
            }
        }
    }

    /// The actual connect sequence, wrapped by [`Self::connect_inner`] in a
    /// single overall deadline. `stage` records which step is in flight so a
    /// timeout can report where it happened; it must be a plain atomic (not
    /// e.g. a `Cell`) so the wrapping future stays `Send` — needed since
    /// callers commonly run `Client::connect` inside `tokio::spawn` (e.g.
    /// `src/bin/swarm.rs`, running many bots on one runtime).
    async fn connect_stages(
        cfg: &ClientConfig,
        trace: Option<TraceRecorder>,
        stage: &AtomicU8,
    ) -> Result<Client> {
        info!(host = %cfg.host, port = cfg.port, version = %cfg.version.minecraft, "connecting");
        stage.store(ConnectStage::TcpConnect as u8, Ordering::Relaxed);
        let timeouts = ConnectionTimeouts {
            write: cfg.write_timeout,
            ..ConnectionTimeouts::default()
        };
        let mut conn = Connection::connect_with_timeouts(&cfg.host, cfg.port, timeouts).await?;
        if let Some(trace) = trace {
            conn.set_trace(trace);
        }

        stage.store(ConnectStage::Handshake as u8, Ordering::Relaxed);
        handshake::send(&mut conn, cfg.version.protocol, &cfg.host, cfg.port).await?;
        conn.set_state(ConnectionState::Login);
        info!("handshake sent, entering login state");

        stage.store(ConnectStage::Login as u8, Ordering::Relaxed);
        let success = login::login(&mut conn, &cfg.username, cfg.premium.as_ref()).await?;
        info!("entering configuration state");

        stage.store(ConnectStage::Configuration as u8, Ordering::Relaxed);
        let configuration = configuration::run_configuration(&mut conn, cfg.view_distance).await?;
        info!("entering play state");

        let (control_tx, control_rx) = channel();
        let (state_tx, state_rx) = watch::channel(StateSnapshot::default());
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(Client {
            conn,
            uuid: success.uuid,
            username: success.username,
            configuration,
            control_tx,
            control_rx: Some(control_rx),
            state_tx: Some(state_tx),
            state_rx,
            event_tx,
        })
    }

    /// Returns a cloneable handle for driving the bot (movement, look,
    /// `walk_to`) while [`run`](Self::run) is executing.
    pub fn control(&self) -> ControlHandle {
        self.control_tx.clone()
    }

    /// Returns a cloneable, always-current view of play state (position,
    /// health, inventory, tracked entities, and presentation state), the
    /// read-side counterpart to
    /// [`Client::control`]. `.borrow()` for the latest snapshot without
    /// blocking, or `.changed().await` to wait for the next update — updates
    /// are published after every clientbound packet and at the end of every
    /// tick while [`run`](Self::run) is executing. Empty/default before the
    /// play loop has published its first snapshot.
    pub fn bot_state(&self) -> watch::Receiver<StateSnapshot> {
        self.state_rx.clone()
    }

    /// Subscribes to the bot's event stream (chat, health changes, death,
    /// tab-list joins/leaves, kicks, ...), the push counterpart to
    /// [`bot_state`](Self::bot_state). Each subscriber gets every event
    /// broadcast from the moment it subscribes onward; a subscriber that
    /// falls behind sees [`tokio::sync::broadcast::error::RecvError::Lagged`]
    /// rather than slowing the play loop down. Can be called before or after
    /// [`run`](Self::run) starts.
    pub fn events(&self) -> broadcast::Receiver<BotEvent> {
        self.event_tx.subscribe()
    }

    /// Runs the play-state loop until the connection errors or the server
    /// disconnects us. Consumes the control and state-publish channels; a
    /// second call runs without an external control channel or state feed.
    pub async fn run(&mut self) -> Result<()> {
        let control_rx = self.control_rx.take().unwrap_or_else(|| channel().1);
        let state_tx = self
            .state_tx
            .take()
            .unwrap_or_else(|| watch::channel(StateSnapshot::default()).0);
        play::run_play(
            &mut self.conn,
            &self.configuration,
            control_rx,
            state_tx,
            self.event_tx.clone(),
        )
        .await
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
