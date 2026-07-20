//! `refresh_token` grant. Zitadel may rotate the refresh token, so the caller
//! must persist whatever comes back; [`device::TokenResponse::into_set`] carries
//! the previous refresh token forward when the response omits a new one.
//!
//! [`device::TokenResponse`] and its `into_set` are shared with [`device`] so
//! the token-response parsing and `TokenSet` materialization live in one place.

use super::device::{TokenResponse, TokenSet, safe_oauth_error_code};
use super::http::HttpClient;
use super::storage::now_unix;
use crate::error::{Error, Result};

/// Exchanges `refresh_token` for a fresh access token at `token_endpoint`.
pub fn refresh(
    http: &HttpClient,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    let resp = http.post_form(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
        None,
    )?;

    if !resp.is_success() {
        let code = safe_oauth_error_code(&resp.body);
        return Err(Error::Auth(format!(
            "token refresh failed ({}; OAuth error {code})",
            resp.status
        )));
    }

    let token: TokenResponse = resp.json("refresh")?;
    Ok(token.into_set(now_unix(), Some(refresh_token)))
}
