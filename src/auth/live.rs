//! The legacy `login.live.com` device-code flow, authenticating as a
//! pre-approved Microsoft/Mojang first-party "Title" (e.g. Minecraft for
//! Nintendo Switch) instead of a user-registered Azure AD application.
//!
//! This is what actually works without extra approval: the modern
//! `login.microsoftonline.com` flow in [`crate::auth::microsoft`] is
//! correctly implemented per the OAuth2/RFC 8628 spec, but Microsoft gates
//! the `XboxLive.signin` scope behind Xbox Developer Program (ID@Xbox)
//! enrollment for user-registered apps — confirmed live: a freshly
//! registered personal Azure app got `403 Invalid app registration` from
//! Minecraft Services even after a successful Microsoft sign-in. First-party
//! titles Microsoft/Mojang already ship (console Minecraft clients) don't
//! have that restriction, which is why bot frameworks like Mineflayer
//! (via `prismarine-auth`) default to borrowing one of those Title ids
//! instead of asking users to register anything.
//!
//! The XBL/XSTS/Minecraft Services hops downstream of this are identical
//! either way — a Live access token is a Live access token regardless of
//! which flow produced it — so only this module differs from `microsoft.rs`.

use std::time::Duration;

use serde::Deserialize;

use crate::core::error::{MineRiderError, Result};

/// Well-known Microsoft/Mojang first-party Title ids, pre-approved for
/// `XboxLive.signin` without any app registration. Mirrors the values
/// `prismarine-auth` ships in its `Titles` export.
pub mod titles {
    pub const MINECRAFT_NINTENDO_SWITCH: &str = "00000000441cc96b";
    pub const MINECRAFT_PLAYSTATION: &str = "000000004827c78e";
    pub const MINECRAFT_ANDROID: &str = "0000000048183522";
    pub const MINECRAFT_JAVA: &str = "00000000402b5328";
    pub const MINECRAFT_IOS: &str = "000000004c17c01a";
}

/// The default title: matches `prismarine-auth`'s own default, and is the
/// combination known to work for Java Edition session-server joins.
pub const DEFAULT_TITLE: &str = titles::MINECRAFT_NINTENDO_SWITCH;

pub const DEVICE_CODE_URL: &str = "https://login.live.com/oauth20_connect.srf";
pub const TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";

/// The Xbox Live "delegation" scope this flow requests — distinct from the
/// modern flow's `XboxLive.signin`, and the one that's actually granted to
/// first-party titles without extra approval.
const SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";

/// The device-code endpoint's response. Same shape as the modern flow's
/// (RFC 8628-like), but from a different, unauthenticated-registration
/// endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl DeviceCodeResponse {
    /// A ready-to-print sign-in instruction, matching the message
    /// `prismarine-auth` constructs (the raw endpoint doesn't provide one,
    /// unlike the modern flow).
    pub fn message(&self) -> String {
        format!(
            "To sign in, use a web browser to open the page {} and enter the code {} (or visit https://microsoft.com/link?otc={})",
            self.verification_uri, self.user_code, self.user_code
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct PollErrorResponse {
    error: String,
    error_description: Option<String>,
}

enum PollOutcome {
    Done(LiveToken),
    Pending,
}

/// Requests a device code, authenticating as `title_id`. The caller's
/// `client` must have cookies enabled (`cookie_store(true)`): the endpoint
/// ties the device code to a session cookie that must be replayed on every
/// poll, or polling fails outright.
pub async fn request_device_code(
    client: &reqwest::Client,
    title_id: &str,
) -> Result<DeviceCodeResponse> {
    let response = client
        .post(DEVICE_CODE_URL)
        .form(&[
            ("scope", SCOPE),
            ("client_id", title_id),
            ("response_type", "device_code"),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(MineRiderError::Auth(format!(
            "device code request failed (HTTP {status}): {body}"
        )));
    }
    parse_device_code_response(&body)
}

fn parse_device_code_response(body: &str) -> Result<DeviceCodeResponse> {
    serde_json::from_str(body)
        .map_err(|e| MineRiderError::Auth(format!("invalid device code response ({e}): {body}")))
}

/// Polls until the user finishes signing in or the device code expires.
/// Requires the *same* `client` (same cookie jar) used for
/// [`request_device_code`].
pub async fn poll_for_token(
    client: &reqwest::Client,
    title_id: &str,
    device: &DeviceCodeResponse,
) -> Result<LiveToken> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    let interval = Duration::from_secs(device.interval.max(1));

    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(MineRiderError::Auth(
                "device code expired before sign-in completed".to_string(),
            ));
        }

        let response = client
            .post(format!("{TOKEN_URL}?client_id={title_id}"))
            .form(&[
                ("client_id", title_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let body = response.text().await?;
        match parse_poll_response(&body)? {
            PollOutcome::Done(token) => return Ok(token),
            PollOutcome::Pending => {}
        }
    }
}

fn parse_poll_response(body: &str) -> Result<PollOutcome> {
    if let Ok(token) = serde_json::from_str::<LiveToken>(body) {
        return Ok(PollOutcome::Done(token));
    }
    let error: PollErrorResponse = serde_json::from_str(body)
        .map_err(|e| MineRiderError::Auth(format!("invalid token poll response ({e}): {body}")))?;
    if error.error == "authorization_pending" {
        return Ok(PollOutcome::Pending);
    }
    Err(MineRiderError::Auth(format!(
        "device code poll failed: {} - {}",
        error.error,
        error.error_description.unwrap_or_default()
    )))
}

/// Refreshes a Live token using a previously obtained refresh token.
pub async fn refresh_token(
    client: &reqwest::Client,
    title_id: &str,
    refresh_token: &str,
) -> Result<LiveToken> {
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("scope", SCOPE),
            ("client_id", title_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;
    let body = response.text().await?;
    serde_json::from_str(&body)
        .map_err(|e| MineRiderError::Auth(format!("invalid refresh response ({e}): {body}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_code_response() {
        let body = r#"{
            "device_code": "abc",
            "user_code": "ABCDEFGH",
            "verification_uri": "https://www.microsoft.com/link",
            "expires_in": 900,
            "interval": 5
        }"#;
        let parsed = parse_device_code_response(body).unwrap();
        assert_eq!(parsed.user_code, "ABCDEFGH");
        assert!(parsed.message().contains("ABCDEFGH"));
        assert!(parsed.message().contains("microsoft.com"));
    }

    #[test]
    fn poll_response_recognizes_pending() {
        let body = r#"{"error":"authorization_pending","error_description":"..."}"#;
        assert!(matches!(
            parse_poll_response(body).unwrap(),
            PollOutcome::Pending
        ));
    }

    #[test]
    fn poll_response_recognizes_success() {
        let body = r#"{
            "token_type": "bearer",
            "expires_in": 3600,
            "access_token": "ey.token",
            "refresh_token": "ey.refresh"
        }"#;
        let PollOutcome::Done(token) = parse_poll_response(body).unwrap() else {
            panic!("expected a completed token");
        };
        assert_eq!(token.access_token, "ey.token");
        assert_eq!(token.refresh_token, "ey.refresh");
    }

    #[test]
    fn poll_response_rejects_other_errors() {
        let body = r#"{"error":"expired_token","error_description":"too slow"}"#;
        assert!(parse_poll_response(body).is_err());
    }
}
