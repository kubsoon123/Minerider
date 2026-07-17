//! Microsoft/Xbox Live/Minecraft Services authentication: the OAuth2 device
//! code flow, Xbox Live and XSTS token exchange, Minecraft Services login,
//! and the Mojang session-server join call that online-mode servers require.
//!
//! This is entirely separate from the offline-mode path in
//! `minecraft::login`: offline mode never touches any of this and keeps
//! working exactly as before. A [`PremiumSession`] plugs into
//! [`crate::minecraft::login::login`] to supply the real username/UUID and,
//! at the right point in the encryption exchange, the join-server call.

pub mod live;
pub mod microsoft;
pub mod minecraft_services;
pub mod server_hash;
pub mod session;
pub mod xbox;

pub use server_hash::server_id_hash;

use crate::core::error::Result;

/// A completed premium login: everything [`crate::minecraft::login::login`]
/// needs to identify as a real account and pass an online-mode server's
/// session check.
#[derive(Debug, Clone)]
pub struct PremiumSession {
    /// Minecraft Services access token, sent to the session server's `join`
    /// endpoint during the encryption exchange.
    pub access_token: String,
    /// The real profile UUID (parsed from the undashed hex the profile API
    /// returns).
    pub uuid: u128,
    /// The real username; the offline-mode username passed to `login()` is
    /// ignored once a `PremiumSession` is supplied.
    pub username: String,
    /// The Microsoft OAuth refresh token: save this to skip the device-code
    /// prompt on a later run via [`MicrosoftAuthenticator::resume`].
    pub refresh_token: String,
    /// Reused for the session-server join call rather than opening a fresh
    /// HTTP client mid-handshake.
    http: reqwest::Client,
    /// The session-server join URL; always [`session::JOIN_URL`] outside
    /// tests, which point it at a local mock to verify the join actually
    /// happens (and with the right fields) without a live Microsoft account.
    session_url: String,
}

/// Runs the Microsoft/Xbox Live/Minecraft Services sign-in flow.
///
/// Uses the legacy `login.live.com` device-code flow (see [`live`]) with a
/// pre-approved Microsoft/Mojang first-party Title id by default — no Azure
/// app registration or Xbox Developer Program approval needed. This is a
/// deliberate choice, not an oversight: the "proper" modern
/// `login.microsoftonline.com` flow is implemented in [`microsoft`] and is
/// spec-correct, but confirmed live to fail for a freshly registered
/// personal Azure app (`403 Invalid app registration`) because
/// `XboxLive.signin` requires Xbox Developer Program enrollment most
/// individual app registrations don't have — the same reason bot frameworks
/// like Mineflayer (via `prismarine-auth`) default to borrowing a Title id
/// instead.
pub struct MicrosoftAuthenticator {
    client: reqwest::Client,
    title_id: String,
}

impl Default for MicrosoftAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrosoftAuthenticator {
    /// Creates an authenticator using [`live::DEFAULT_TITLE`] — works with
    /// no setup.
    pub fn new() -> Self {
        Self::with_title(live::DEFAULT_TITLE)
    }

    /// Like [`new`](Self::new), but against a caller-chosen Title id (see
    /// [`live::titles`]).
    pub fn with_title(title_id: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .cookie_store(true) // the device-code endpoint ties the code to a session cookie
            .build()
            .expect("reqwest client with cookie store");
        Self {
            client,
            title_id: title_id.into(),
        }
    }

    /// Runs the full device-code sign-in flow: requests a code, calls
    /// `on_user_code` once with it (print it, show a dialog, ...) so the
    /// user can approve sign-in in a browser, then polls until they do and
    /// completes Xbox Live → XSTS → Minecraft Services → profile fetch,
    /// verifying game ownership along the way.
    pub async fn sign_in(
        &self,
        on_user_code: impl FnOnce(&live::DeviceCodeResponse),
    ) -> Result<PremiumSession> {
        let device = live::request_device_code(&self.client, &self.title_id).await?;
        on_user_code(&device);
        let token = live::poll_for_token(&self.client, &self.title_id, &device).await?;
        self.finish_sign_in(token).await
    }

    /// Resumes a session from a previously saved Microsoft refresh token,
    /// skipping the device-code prompt entirely.
    pub async fn resume(&self, refresh_token: &str) -> Result<PremiumSession> {
        let token = live::refresh_token(&self.client, &self.title_id, refresh_token).await?;
        self.finish_sign_in(token).await
    }

    async fn finish_sign_in(&self, token: live::LiveToken) -> Result<PremiumSession> {
        let xbl =
            xbox::authenticate_xbl(&self.client, &token.access_token, xbox::TokenSource::Live)
                .await?;
        let xsts = xbox::authorize_xsts(&self.client, &xbl.token).await?;
        let mc_token =
            minecraft_services::login_with_xbox(&self.client, &xsts.user_hash, &xsts.token).await?;
        minecraft_services::verify_game_ownership(&self.client, &mc_token).await?;
        let profile = minecraft_services::fetch_profile(&self.client, &mc_token).await?;
        Ok(PremiumSession {
            access_token: mc_token.access_token,
            uuid: profile.uuid,
            username: profile.username,
            refresh_token: token.refresh_token,
            http: self.client.clone(),
            session_url: session::JOIN_URL.to_string(),
        })
    }
}

impl PremiumSession {
    /// Builds a session with an overridden session-server URL, for tests
    /// that need to verify the join call against a local mock instead of
    /// the real `sessionserver.mojang.com` (no live Microsoft account
    /// needed to check the wiring, only the value correctness).
    #[cfg(test)]
    pub(crate) fn for_test(
        access_token: impl Into<String>,
        uuid: u128,
        username: impl Into<String>,
        session_url: impl Into<String>,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            uuid,
            username: username.into(),
            refresh_token: String::new(),
            http: reqwest::Client::new(),
            session_url: session_url.into(),
        }
    }

    /// Calls the session server's `join` endpoint so this account's
    /// `hasJoined` check succeeds during the encryption exchange. Must run
    /// after the shared secret is known but before the `encryption_begin`
    /// response is sent.
    pub(crate) async fn join_session(
        &self,
        server_id: &str,
        shared_secret: &[u8],
        public_key_der: &[u8],
    ) -> Result<()> {
        let hash = server_id_hash(server_id, shared_secret, public_key_der);
        session::join_to(
            &self.http,
            &self.session_url,
            &self.access_token,
            self.uuid,
            &hash,
        )
        .await
    }
}
