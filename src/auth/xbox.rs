//! Xbox Live user authentication and XSTS token exchange: the hop between a
//! Microsoft OAuth token and a Minecraft Services login.

use serde::{Deserialize, Serialize};

use crate::core::error::{MineRiderError, Result};

pub const XBL_AUTHENTICATE_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
pub const XSTS_AUTHORIZE_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";

/// The relying party Minecraft Services expects the XSTS token to be issued
/// for; using the wrong one is a common, silent way to get a token Minecraft
/// Services then rejects.
const MINECRAFT_RELYING_PARTY: &str = "rp://api.minecraftservices.com/";

/// A successful Xbox Live or XSTS token, both of which share this shape.
#[derive(Debug, Clone)]
pub struct XboxToken {
    pub token: String,
    /// The user hash (`uhs`) from `DisplayClaims.xui[0].uhs`, combined with
    /// the token to form Minecraft Services' `identityToken`.
    pub user_hash: String,
}

#[derive(Debug, Deserialize)]
struct XboxResponseBody {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: DisplayClaims,
}

#[derive(Debug, Deserialize)]
struct DisplayClaims {
    xui: Vec<Xui>,
}

#[derive(Debug, Deserialize)]
struct Xui {
    uhs: String,
}

#[derive(Debug, Deserialize)]
struct XboxErrorBody {
    #[serde(rename = "XErr")]
    x_err: Option<u64>,
}

#[derive(Serialize)]
struct XblAuthenticateRequest {
    #[serde(rename = "Properties")]
    properties: XblAuthenticateProperties,
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
}

#[derive(Serialize)]
struct XblAuthenticateProperties {
    #[serde(rename = "AuthMethod")]
    auth_method: &'static str,
    #[serde(rename = "SiteName")]
    site_name: &'static str,
    #[serde(rename = "RpsTicket")]
    rps_ticket: String,
}

#[derive(Serialize)]
struct XstsAuthorizeRequest<'a> {
    #[serde(rename = "Properties")]
    properties: XstsAuthorizeProperties<'a>,
    #[serde(rename = "RelyingParty")]
    relying_party: &'static str,
    #[serde(rename = "TokenType")]
    token_type: &'static str,
}

#[derive(Serialize)]
struct XstsAuthorizeProperties<'a> {
    #[serde(rename = "SandboxId")]
    sandbox_id: &'static str,
    #[serde(rename = "UserTokens")]
    user_tokens: &'a [&'a str],
}

/// Which OAuth flow produced the Microsoft access token being exchanged.
/// Xbox Live's `RpsTicket` needs a different preamble depending on this —
/// `d=` for the modern `login.microsoftonline.com` flow, `t=` for the
/// legacy `login.live.com` one ([`crate::auth::live`], what
/// [`crate::auth::MicrosoftAuthenticator`] uses by default). Sending the
/// wrong one is a silent way to get a bare `401` with no error body,
/// confirmed live against the real service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Azure,
    Live,
}

impl TokenSource {
    fn rps_preamble(self) -> &'static str {
        match self {
            TokenSource::Azure => "d=",
            TokenSource::Live => "t=",
        }
    }
}

/// Signs in to Xbox Live with a Microsoft OAuth access token, returning the
/// XBL user token used as input to [`authorize_xsts`].
pub async fn authenticate_xbl(
    client: &reqwest::Client,
    ms_access_token: &str,
    source: TokenSource,
) -> Result<XboxToken> {
    let request = XblAuthenticateRequest {
        properties: XblAuthenticateProperties {
            auth_method: "RPS",
            site_name: "user.auth.xboxlive.com",
            rps_ticket: format!("{}{ms_access_token}", source.rps_preamble()),
        },
        relying_party: "http://auth.xboxlive.com",
        token_type: "JWT",
    };
    let response = client
        .post(XBL_AUTHENTICATE_URL)
        .json(&request)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    parse_xbox_response(status, &body, "Xbox Live sign-in")
}

/// Exchanges an XBL user token for the XSTS token Minecraft Services accepts.
pub async fn authorize_xsts(client: &reqwest::Client, xbl_token: &str) -> Result<XboxToken> {
    let request = XstsAuthorizeRequest {
        properties: XstsAuthorizeProperties {
            sandbox_id: "RETAIL",
            user_tokens: &[xbl_token],
        },
        relying_party: MINECRAFT_RELYING_PARTY,
        token_type: "JWT",
    };
    let response = client
        .post(XSTS_AUTHORIZE_URL)
        .json(&request)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    parse_xbox_response(status, &body, "XSTS authorization")
}

fn parse_xbox_response(status: reqwest::StatusCode, body: &str, step: &str) -> Result<XboxToken> {
    if status.is_success() {
        let parsed: XboxResponseBody = serde_json::from_str(body)
            .map_err(|e| MineRiderError::Auth(format!("invalid {step} response ({e}): {body}")))?;
        let user_hash = parsed
            .display_claims
            .xui
            .first()
            .map(|xui| xui.uhs.clone())
            .ok_or_else(|| MineRiderError::Auth(format!("{step} response had no user hash")))?;
        return Ok(XboxToken {
            token: parsed.token,
            user_hash,
        });
    }
    // A failed XSTS authorization carries a numeric XErr the wiki documents;
    // surface the specific, actionable reason instead of a bare HTTP status.
    if let Ok(error) = serde_json::from_str::<XboxErrorBody>(body) {
        if let Some(message) = error.x_err.and_then(describe_xerr) {
            return Err(MineRiderError::Auth(format!("{step} failed: {message}")));
        }
    }
    Err(MineRiderError::Auth(format!(
        "{step} failed (HTTP {status}): {body}"
    )))
}

/// Human-readable explanations for the XSTS `XErr` codes Microsoft
/// documents; `None` for anything not in that documented set.
fn describe_xerr(code: u64) -> Option<&'static str> {
    match code {
        2148916233 => Some(
            "this Microsoft account has no Xbox Live account; create one at https://signup.live.com",
        ),
        2148916235 => Some("Xbox Live is not available in this account's country"),
        2148916236 | 2148916237 => {
            Some("adult verification is required on this Xbox Live account (South Korea)")
        }
        2148916238 => Some(
            "this is a child account and must be added to a Microsoft family group before it can sign in",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_xbox_response() {
        let body = r#"{
            "IssueInstant": "2020-01-01T00:00:00Z",
            "NotAfter": "2020-01-01T01:00:00Z",
            "Token": "xbl-token-value",
            "DisplayClaims": { "xui": [ { "uhs": "user-hash-value" } ] }
        }"#;
        let token = parse_xbox_response(reqwest::StatusCode::OK, body, "test").unwrap();
        assert_eq!(token.token, "xbl-token-value");
        assert_eq!(token.user_hash, "user-hash-value");
    }

    #[test]
    fn maps_known_xerr_to_actionable_message() {
        let body = r#"{"Identity":"0","XErr":2148916233,"Message":"","Redirect":""}"#;
        let err = parse_xbox_response(
            reqwest::StatusCode::UNAUTHORIZED,
            body,
            "XSTS authorization",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("no Xbox Live account"));
    }

    #[test]
    fn unknown_failure_still_surfaces_status_and_body() {
        let err = parse_xbox_response(reqwest::StatusCode::FORBIDDEN, "nope", "test").unwrap_err();
        assert!(format!("{err}").contains("403"));
    }

    #[test]
    fn missing_user_hash_is_an_error_not_a_panic() {
        let body = r#"{"Token":"t","DisplayClaims":{"xui":[]}}"#;
        assert!(parse_xbox_response(reqwest::StatusCode::OK, body, "test").is_err());
    }
}
