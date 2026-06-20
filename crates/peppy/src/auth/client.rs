//! Authenticated calls to the `platform-backend` resource server: `GET /me` and
//! `POST /logout`. On a `401` with a refreshable session credential the request
//! is retried once after refreshing (and persisting) the token; a `401` on a PAT
//! is a hard error (a PAT cannot be refreshed). `502`/`503` map to distinct
//! messages so an ops problem isn't mistaken for a bad token.

use secrecy::ExposeSecret;
use serde::Deserialize;

use super::http::{HttpClient, HttpResponse};
use super::resolver::{Credential, CredentialKind};
use super::storage::{self, ProfileCreds};
use super::{discovery, refresh};
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

/// Refreshes a session credential in place: discovers the token endpoint from the
/// cached issuer, exchanges the refresh token, updates the credential, and
/// persists the rotation to the credentials file.
fn refresh_in_place(http: &HttpClient, cred: &mut Credential) -> Result<()> {
    let CredentialKind::Session(ctx) = &cred.kind else {
        return Ok(());
    };
    let endpoints = discovery::discover(http, &ctx.issuer)?;
    let tokens = refresh::refresh(
        http,
        &endpoints.token_endpoint,
        &ctx.client_id,
        ctx.refresh_token.expose_secret(),
    )?;

    // Persist the rotation against the stored session (if still present).
    let mut creds = storage::load(&ctx.creds_path)?;
    if let Some(existing) = creds.session.as_ref() {
        let updated = super::resolver::apply_tokens(existing, &tokens);
        creds.session = Some(updated);
        storage::save(&ctx.creds_path, &creds)?;
    }

    cred.token = storage::secret(tokens.access_token.clone());
    cred.kind = CredentialKind::Session(super::resolver::SessionContext {
        issuer: ctx.issuer.clone(),
        client_id: ctx.client_id.clone(),
        refresh_token: storage::secret(tokens.refresh_token.clone()),
        creds_path: ctx.creds_path.clone(),
    });
    Ok(())
}

/// The 401 message, specialized by credential kind.
fn unauthorized_error(cred: &Credential) -> Error {
    match cred.kind {
        CredentialKind::Pat => {
            Error::Auth("API key rejected (revoked or expired?) — cannot refresh".to_string())
        }
        CredentialKind::Session(_) => Error::NotAuthenticated,
    }
}

/// Builds the [`ProfileCreds`] to persist after a fresh login.
pub fn creds_from_login(
    cfg: &super::cli_config::CliConfig,
    api_url: &str,
    tokens: &super::device::TokenSet,
    principal: Option<&Principal>,
) -> ProfileCreds {
    ProfileCreds {
        api_url: api_url.trim_end_matches('/').to_string(),
        issuer: cfg.issuer.clone(),
        client_id: cfg.client_id.clone(),
        access_token: storage::secret(tokens.access_token.clone()),
        refresh_token: storage::secret(tokens.refresh_token.clone()),
        expires_at: tokens.expires_at,
        token_type: tokens.token_type.clone(),
        scope: tokens.scope.clone(),
        subject: principal.map(|p| p.sub.clone()).unwrap_or_default(),
        username: principal
            .map(|p| p.display_name().to_string())
            .unwrap_or_default(),
    }
}
