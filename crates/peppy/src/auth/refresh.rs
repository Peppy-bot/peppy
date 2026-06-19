//! `refresh_token` grant. Zitadel may rotate the refresh token, so the caller
//! must persist whatever comes back; [`device::TokenResponse::into_set`] carries
//! the previous refresh token forward when the response omits a new one.

use serde::Deserialize;

use super::device::TokenSet;
use super::http;
use super::storage::now_unix;
use crate::error::{Error, Result};

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Exchanges `refresh_token` for a fresh access token at `token_endpoint`.
pub fn refresh(
    agent: &ureq::Agent,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    let resp = http::post_form(
        agent,
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
        None,
    )?;

    if !resp.is_success() {
        return Err(Error::Auth(format!(
            "token refresh failed ({}): {}",
            resp.status, resp.body
        )));
    }

    let token: TokenResponse = serde_json::from_str(&resp.body)
        .map_err(|e| Error::Auth(format!("invalid refresh response: {e}")))?;
    let now = now_unix();
    let next_refresh = token
        .refresh_token
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| refresh_token.to_string());

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: next_refresh,
        expires_at: now + token.expires_in,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        scope: token.scope.unwrap_or_default(),
    })
}
