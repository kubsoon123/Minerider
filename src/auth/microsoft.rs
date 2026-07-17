//! Microsoft identity platform OAuth2 device-code flow (RFC 8628-shaped,
//! `login.microsoftonline.com`) for a user-registered Azure AD application.
//!
//! **Not currently used by [`crate::auth::MicrosoftAuthenticator`]**, which
//! defaults to [`crate::auth::live`] instead. This module is spec-correct
//! and unit-tested, but confirmed live to fail for a freshly registered
//! personal Azure app: Minecraft Services returns `403 Invalid app
//! registration` even after a successful Microsoft sign-in, because
//! `XboxLive.signin` requires Xbox Developer Program (ID@Xbox) enrollment —
//! a formal game-submission process, not an Azure portal setting — that
//! most individual app registrations don't have. Kept for anyone who *does*
//! have that approval (`MicrosoftAuthenticator` has no constructor wired to
//! it right now; use this module's functions directly).

use std::time::Duration;

use serde::Deserialize;

use crate::core::error::{MineRiderError, Result};

/// Default (consumers-tenant) Microsoft identity platform base. Overridable
/// so tests can point at a local mock server.
pub const DEFAULT_MS_BASE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

/// The scope requested: sign in to Xbox Live, plus a refresh token so a
/// session can be renewed without repeating the device-code dance.
const SCOPE: &str = "XboxLive.signin offline_access";

/// The device-code grant type, per RFC 8628.
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The server's response to a device-code request: what to show the user and
/// what to poll with.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    /// A ready-to-print instruction string ("To sign in, use a web browser
    /// to open the page ... and enter the code ... to authenticate").
    pub message: String,
}

/// A completed Microsoft OAuth token (the input to the Xbox Live hop, not a
/// Minecraft credential by itself).
#[derive(Debug, Clone, Deserialize)]
pub struct MicrosoftToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    error: String,
}

/// Outcome of one poll of the token endpoint while the user hasn't finished
/// signing in yet.
enum PollOutcome {
    Done(MicrosoftToken),
    Pending,
    SlowDown,
}

/// Requests a device code from the Microsoft identity platform.
pub async fn request_device_code(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
) -> Result<DeviceCodeResponse> {
    let response = client
        .post(format!("{base}/devicecode"))
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .await?;
    let body = response.text().await?;
    parse_device_code_response(&body)
}

fn parse_device_code_response(body: &str) -> Result<DeviceCodeResponse> {
    serde_json::from_str(body)
        .map_err(|e| MineRiderError::Auth(format!("invalid device code response ({e}): {body}")))
}

/// Polls the token endpoint until the user finishes signing in, honoring the
/// server's requested interval (and any `slow_down` backoff), or fails once
/// the device code's own `expires_in` window has elapsed.
pub async fn poll_for_token(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    device: &DeviceCodeResponse,
) -> Result<MicrosoftToken> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));

    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(MineRiderError::Auth(
                "device code expired before sign-in completed".to_string(),
            ));
        }

        let response = client
            .post(format!("{base}/token"))
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("client_id", client_id),
                ("device_code", &device.device_code),
            ])
            .send()
            .await?;
        let body = response.text().await?;
        match parse_poll_response(&body)? {
            PollOutcome::Done(token) => return Ok(token),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += Duration::from_secs(5),
        }
    }
}

fn parse_poll_response(body: &str) -> Result<PollOutcome> {
    if let Ok(token) = serde_json::from_str::<MicrosoftToken>(body) {
        return Ok(PollOutcome::Done(token));
    }
    let error: TokenErrorResponse = serde_json::from_str(body)
        .map_err(|e| MineRiderError::Auth(format!("invalid token poll response ({e}): {body}")))?;
    match error.error.as_str() {
        "authorization_pending" => Ok(PollOutcome::Pending),
        "slow_down" => Ok(PollOutcome::SlowDown),
        "authorization_declined" => Err(MineRiderError::Auth(
            "sign-in was declined at the Microsoft prompt".to_string(),
        )),
        "expired_token" => Err(MineRiderError::Auth(
            "device code expired before sign-in completed".to_string(),
        )),
        "bad_verification_code" => Err(MineRiderError::Auth(
            "Microsoft rejected the device code (bad_verification_code)".to_string(),
        )),
        other => Err(MineRiderError::Auth(format!(
            "device code poll failed: {other}"
        ))),
    }
}

/// Refreshes a Microsoft token using a previously obtained refresh token,
/// skipping the device-code prompt entirely on subsequent runs.
pub async fn refresh_token(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<MicrosoftToken> {
    let response = client
        .post(format!("{base}/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
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
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/link",
            "expires_in": 900,
            "interval": 5,
            "message": "To sign in, use a web browser..."
        }"#;
        let parsed = parse_device_code_response(body).unwrap();
        assert_eq!(parsed.user_code, "ABCD-EFGH");
        assert_eq!(parsed.interval, 5);
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
    fn poll_response_recognizes_slow_down() {
        let body = r#"{"error":"slow_down"}"#;
        assert!(matches!(
            parse_poll_response(body).unwrap(),
            PollOutcome::SlowDown
        ));
    }

    #[test]
    fn poll_response_rejects_declined() {
        let body = r#"{"error":"authorization_declined"}"#;
        assert!(parse_poll_response(body).is_err());
    }

    #[test]
    fn poll_response_recognizes_success() {
        let body = r#"{
            "token_type": "Bearer",
            "scope": "XboxLive.signin offline_access",
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
}
