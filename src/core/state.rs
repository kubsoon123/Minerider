//! Connection state machine shared by the network and minecraft layers.

/// The protocol state of a Minecraft connection.
///
/// Transitions: `Handshaking` → `Login` → `Configuration` → `Play`
/// (or `Handshaking` → `Status` for server-list pings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Handshaking,
    Status,
    Login,
    Configuration,
    Play,
}
