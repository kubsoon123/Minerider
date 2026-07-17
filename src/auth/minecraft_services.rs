//! Minecraft Services (`api.minecraftservices.com`): trading an XSTS token
//! for a Minecraft access token, checking game ownership, and fetching the
//! real profile (UUID, username) a premium client identifies as.

use serde::{Deserialize, Serialize};

use crate::core::error::{MineRiderError, Result};

const LOGIN_WITH_XBOX_URL: &str =
    "https://api.minecraftservices.com/authentication/login_with_xbox";
const ENTITLEMENTS_URL: &str = "https://api.minecraftservices.com/entitlements/mcstore";
const PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

/// A Minecraft Services access token: the Bearer credential for the
/// entitlement/profile checks and, ultimately, the session-server join call.
#[derive(Debug, Clone)]
pub struct MinecraftToken {
    pub access_token: String,
}

/// The real premium profile a client identifies as: the identity that
/// replaces MineRider's offline all-zero UUID once authenticated.
#[derive(Debug, Clone, PartialEq)]
pub struct MinecraftProfile {
    pub uuid: u128,
    pub username: String,
}

#[derive(Serialize)]
struct LoginWithXboxRequest {
    #[serde(rename = "identityToken")]
    identity_token: String,
}

#[derive(Debug, Deserialize)]
struct LoginWithXboxResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct EntitlementsResponse {
    items: Vec<EntitlementItem>,
}

#[derive(Debug, Deserialize)]
struct EntitlementItem {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    #[serde(alias = "errorMessage", alias = "error")]
    message: Option<String>,
}

/// Trades an XSTS token + user hash for a Minecraft Services access token.
pub async fn login_with_xbox(
    client: &reqwest::Client,
    user_hash: &str,
    xsts_token: &str,
) -> Result<MinecraftToken> {
    let request = LoginWithXboxRequest {
        identity_token: format!("XBL3.0 x={user_hash};{xsts_token}"),
    };
    let response = client
        .post(LOGIN_WITH_XBOX_URL)
        .json(&request)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(auth_error(status, &body, "Minecraft Services login"));
    }
    let parsed: LoginWithXboxResponse = serde_json::from_str(&body).map_err(|e| {
        MineRiderError::Auth(format!(
            "invalid Minecraft Services login response ({e}): {body}"
        ))
    })?;
    Ok(MinecraftToken {
        access_token: parsed.access_token,
    })
}

/// Confirms the account owns Minecraft Java Edition. Vanilla launchers check
/// this before offering play; skipping it just delays the same rejection to
/// the profile fetch, so failing fast here gives a clearer error.
pub async fn verify_game_ownership(client: &reqwest::Client, token: &MinecraftToken) -> Result<()> {
    let response = client
        .get(ENTITLEMENTS_URL)
        .bearer_auth(&token.access_token)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(auth_error(status, &body, "entitlement check"));
    }
    let parsed: EntitlementsResponse = serde_json::from_str(&body).map_err(|e| {
        MineRiderError::Auth(format!("invalid entitlements response ({e}): {body}"))
    })?;
    let owns_game = parsed
        .items
        .iter()
        .any(|item| item.name.contains("minecraft"));
    if owns_game {
        Ok(())
    } else {
        Err(MineRiderError::Auth(
            "this Microsoft account does not own Minecraft: Java Edition".to_string(),
        ))
    }
}

/// Fetches the real profile (UUID, username) this account plays as.
pub async fn fetch_profile(
    client: &reqwest::Client,
    token: &MinecraftToken,
) -> Result<MinecraftProfile> {
    let response = client
        .get(PROFILE_URL)
        .bearer_auth(&token.access_token)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(auth_error(status, &body, "profile fetch"));
    }
    parse_profile_response(&body)
}

fn parse_profile_response(body: &str) -> Result<MinecraftProfile> {
    let parsed: ProfileResponse = serde_json::from_str(body)
        .map_err(|e| MineRiderError::Auth(format!("invalid profile response ({e}): {body}")))?;
    let uuid = u128::from_str_radix(&parsed.id, 16).map_err(|e| {
        MineRiderError::Auth(format!("profile uuid {:?} is not hex ({e})", parsed.id))
    })?;
    Ok(MinecraftProfile {
        uuid,
        username: parsed.name,
    })
}

fn auth_error(status: reqwest::StatusCode, body: &str, step: &str) -> MineRiderError {
    if let Ok(error) = serde_json::from_str::<ErrorResponse>(body) {
        if let Some(message) = error.message {
            return MineRiderError::Auth(format!("{step} failed (HTTP {status}): {message}"));
        }
    }
    MineRiderError::Auth(format!("{step} failed (HTTP {status}): {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profile_uuid_as_undashed_hex() {
        let body = r#"{
            "id": "069a79f444e94726a5befca90e38aaf",
            "name": "Notch",
            "skins": [],
            "capes": []
        }"#;
        let profile = parse_profile_response(body).unwrap();
        assert_eq!(profile.username, "Notch");
        assert_eq!(profile.uuid, 0x069a79f444e94726a5befca90e38aaf);
    }

    #[test]
    fn rejects_non_hex_uuid_instead_of_panicking() {
        let body = r#"{"id":"not-hex-at-all","name":"X","skins":[],"capes":[]}"#;
        assert!(parse_profile_response(body).is_err());
    }

    #[test]
    fn auth_error_prefers_structured_message() {
        let body = r#"{"error":"NOT_FOUND","errorMessage":"The server has not found anything matching the request URI"}"#;
        let err = auth_error(reqwest::StatusCode::NOT_FOUND, body, "profile fetch");
        assert!(format!("{err}").contains("has not found"));
    }
}
