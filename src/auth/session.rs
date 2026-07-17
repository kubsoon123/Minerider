//! The Mojang session-server `join` call: the step an online-mode server
//! verifies against (via its own `hasJoined` call) after the client answers
//! `encryption_begin`. Skipping this — or calling it after, instead of
//! before, sending the encryption response — is the single most common way
//! a from-scratch client fails online-mode login with "Failed to verify
//! username!" even though every other packet was correct.

use crate::core::error::{MineRiderError, Result};

pub const JOIN_URL: &str = "https://sessionserver.mojang.com/session/minecraft/join";

/// Formats a `u128` UUID as the undashed lowercase hex Mojang's session API
/// expects (`selectedProfile`), zero-padded to 32 hex digits.
fn uuid_hex(uuid: u128) -> String {
    format!("{uuid:032x}")
}

/// Tells the session server this client is joining with the given
/// (server-id-hashed) session id, so the server's own `hasJoined` check
/// succeeds. Must be called after the shared secret is known but before the
/// `encryption_begin` response is sent — see [`crate::minecraft::login`].
pub async fn join(
    client: &reqwest::Client,
    access_token: &str,
    profile_uuid: u128,
    server_id_hash: &str,
) -> Result<()> {
    join_to(client, JOIN_URL, access_token, profile_uuid, server_id_hash).await
}

/// Like [`join`], but against a caller-chosen URL — the seam that lets a
/// test point this at a local mock instead of the real session server.
pub(crate) async fn join_to(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    profile_uuid: u128,
    server_id_hash: &str,
) -> Result<()> {
    let body = serde_json::json!({
        "accessToken": access_token,
        "selectedProfile": uuid_hex(profile_uuid),
        "serverId": server_id_hash,
    });
    let response = client.post(url).json(&body).send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.unwrap_or_default();
    Err(MineRiderError::Auth(format!(
        "session server join failed (HTTP {status}): {text}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_hex_is_zero_padded_and_undashed() {
        let uuid = 0x069a79f444e94726a5befca90e38aafu128;
        let hex = uuid_hex(uuid);
        assert_eq!(hex.len(), 32);
        assert_eq!(u128::from_str_radix(&hex, 16).unwrap(), uuid);
        // A UUID with leading zero bytes must not lose them: always 32 hex
        // digits, round-tripping back to the same value.
        let small = uuid_hex(1);
        assert_eq!(small.len(), 32);
        assert!(small.ends_with('1'));
        assert_eq!(u128::from_str_radix(&small, 16).unwrap(), 1);
    }
}
