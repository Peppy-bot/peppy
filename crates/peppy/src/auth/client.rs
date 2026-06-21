//! Authenticated calls to the `platform-backend` resource server: `GET /me` and
//! `POST /logout`. On a `401` with a refreshable session credential the request
//! is retried once after refreshing (and persisting) the token; a `401` on a PAT
//! is a hard error (a PAT cannot be refreshed). `502`/`503` map to distinct
//! messages so an ops problem isn't mistaken for a bad token.

use secrecy::ExposeSecret;
use serde::Deserialize;

use super::http::{HttpClient, HttpResponse};
use super::resolver::{Credential, CredentialKind, refresh_and_persist};
use super::storage::{self, ProfileCreds};
use crate::error::{Error, Result};

/// The identity the backend reports for the current token. Deserialized
/// tolerantly: only `sub` is required, everything else is optional so a backend
/// that adds fields (or omits an optional one) still parses.
#[derive(Debug, Clone, Deserialize)]
pub struct Principal {
    #[serde(default)]
    pub id: Option<String>,
    pub sub: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub owner_principal_id: Option<String>,
}

impl Principal {
    /// A human label for `whoami` / login confirmation: username, else email,
    /// else the subject.
    pub fn display_name(&self) -> &str {
        self.username
            .as_deref()
            .or(self.email.as_deref())
            .unwrap_or(&self.sub)
    }
}

/// `GET {api_url}/me`, refreshing once on a 401 for session credentials.
pub fn get_me(http: &HttpClient, api_url: &str, cred: &mut Credential) -> Result<Principal> {
    let url = format!("{}/me", api_url.trim_end_matches('/'));
    let resp = authed_get(http, &url, cred)?;
    match resp.status {
        200 => resp.json("/me"),
        401 => Err(unauthorized_error(cred)),
        502 => Err(Error::Auth(
            "the backend's introspection credentials were rejected (server-side problem)"
                .to_string(),
        )),
        503 => Err(Error::Http(
            "backend temporarily unavailable, try again shortly".to_string(),
        )),
        s => Err(Error::Http(format!("GET {url} returned {s}"))),
    }
}

/// `POST {api_url}/logout` with the current access token. Returns the status code
/// so the caller can decide what to print; never refreshes (the token is being
/// thrown away regardless).
pub fn logout(http: &HttpClient, api_url: &str, access_token: &str) -> Result<u16> {
    let url = format!("{}/logout", api_url.trim_end_matches('/'));
    let resp = http.post_empty(&url, Some(access_token))?;
    Ok(resp.status)
}

/// A GET that, on 401 with a session credential, refreshes (and persists) the
/// token and retries exactly once.
fn authed_get(http: &HttpClient, url: &str, cred: &mut Credential) -> Result<HttpResponse> {
    let resp = http.get(url, Some(cred.token.expose_secret()))?;
    if resp.status == 401 && cred.is_refreshable() {
        refresh_in_place(http, cred)?;
        return http.get(url, Some(cred.token.expose_secret()));
    }
    Ok(resp)
}

/// Refreshes a session credential in place via the shared refresh-and-persist
/// pipeline, then rebuilds the [`Credential`] from the rotated tokens.
fn refresh_in_place(http: &HttpClient, cred: &mut Credential) -> Result<()> {
    let CredentialKind::Session(ctx) = &cred.kind else {
        return Ok(());
    };

    // Load the stored session to refresh from (it may have changed since the
    // credential was built, e.g. another command refreshed in parallel).
    let creds = storage::load(&ctx.creds_path)?;
    let Some(pc) = creds.session.as_ref() else {
        return Ok(());
    };

    let updated = refresh_and_persist(http, &ctx.creds_path, pc)?;

    cred.token = storage::secret(updated.access_token.expose_secret().to_string());
    cred.kind = CredentialKind::Session(super::resolver::SessionContext {
        issuer: updated.issuer.clone(),
        client_id: updated.client_id.clone(),
        refresh_token: storage::secret(updated.refresh_token.expose_secret().to_string()),
        creds_path: ctx.creds_path.clone(),
    });
    Ok(())
}

/// The 401 message, specialized by credential kind.
fn unauthorized_error(cred: &Credential) -> Error {
    match cred.kind {
        CredentialKind::Pat => {
            Error::Auth("API key rejected (revoked or expired?), cannot refresh".to_string())
        }
        CredentialKind::Session(_) => Error::NotAuthenticated,
    }
}

/// Builds the [`ProfileCreds`] to persist after a fresh login.
pub fn creds_from_login(
    cfg: &super::cli_config::CliConfig,
    api_url: &str,
    tokens: &super::device::TokenSet,
) -> ProfileCreds {
    ProfileCreds::with_tokens(
        api_url.trim_end_matches('/').to_string(),
        cfg.issuer.clone(),
        cfg.client_id.clone(),
        String::new(),
        String::new(),
        tokens,
    )
}
