//! Credential resolution for authenticated commands.
//!
//! Precedence (the interactive device-login step is owned by the `login`
//! command, not the resolver — `whoami`/`logout` never auto-open a browser):
//!
//! 1. `PEPPY_API_KEY` PAT → bearer, no refresh (CI / automation).
//! 2. cached session token, valid → use it.
//! 3. cached session token expired but refreshable → refresh, persist rotation,
//!    use it.
//! 4. otherwise → [`Error::NotAuthenticated`].

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

use super::http::HttpClient;
use super::storage::{self, ProfileCreds};
use super::{discovery, refresh};
use crate::error::{Error, Result};

/// Refresh slightly before the real expiry to avoid racing a just-expired token.
const EXPIRY_SKEW_SECS: i64 = 30;

/// A ready bearer plus the context needed to refresh it on a reactive `401`.
pub struct Credential {
    pub token: SecretString,
    pub kind: CredentialKind,
}

impl Credential {
    pub fn is_refreshable(&self) -> bool {
        matches!(self.kind, CredentialKind::Session(_))
    }
}

/// Whether the bearer can be refreshed.
pub enum CredentialKind {
    /// A `PEPPY_API_KEY` PAT — long-lived, not refreshable.
    Pat,
    /// A cached session token, refreshable via the carried OIDC context.
    Session(SessionContext),
}

/// Everything needed to refresh a session token and persist the rotation.
pub struct SessionContext {
    pub issuer: String,
    pub client_id: String,
    pub refresh_token: SecretString,
    pub creds_path: PathBuf,
}

/// Resolves a usable credential from the single cached session. `pat` is the
/// injected `PEPPY_API_KEY` value (production passes the env var; tests pass it
/// explicitly to avoid env races).
pub fn resolve(creds_path: &Path, http: &HttpClient, pat: Option<String>) -> Result<Credential> {
    if let Some(pat) = pat.filter(|v| !v.is_empty()) {
        return Ok(Credential {
            token: storage::secret(pat),
            kind: CredentialKind::Pat,
        });
    }

    let creds = storage::load(creds_path)?;
    let pc = creds.session.clone().ok_or(Error::NotAuthenticated)?;

    if !pc.is_expired(storage::now_unix(), EXPIRY_SKEW_SECS) {
        return Ok(session_credential(creds_path, &pc));
    }

    // Expired: refresh proactively and persist the rotation.
    let updated = refresh_and_persist(http, creds_path, &pc).map_err(|e| {
        Error::Auth(format!(
            "{e}\nYour session may have expired, run `peppy login`."
        ))
    })?;
    Ok(session_credential(creds_path, &updated))
}

/// Builds a refreshable session [`Credential`] from the cached `pc`.
fn session_credential(creds_path: &Path, pc: &ProfileCreds) -> Credential {
    Credential {
        token: storage::secret(pc.access_token.expose_secret().to_string()),
        kind: CredentialKind::Session(SessionContext {
            issuer: pc.issuer.clone(),
            client_id: pc.client_id.clone(),
            refresh_token: storage::secret(pc.refresh_token.expose_secret().to_string()),
            creds_path: creds_path.to_path_buf(),
        }),
    }
}

/// The single implementation of the refresh pipeline: discovers the token
/// endpoint from the cached issuer, exchanges the refresh token, applies the
/// rotated tokens to the stored session, and persists the result. Returns the
/// updated [`ProfileCreds`] so the caller can build a [`Credential`] from it.
///
/// Both proactive refresh (resolver, on expired-at-load) and reactive refresh
/// (client, on `401`) funnel through here so the discover-refresh-persist
/// contract has one definition.
pub(crate) fn refresh_and_persist(
    http: &HttpClient,
    creds_path: &Path,
    pc: &ProfileCreds,
) -> Result<ProfileCreds> {
    let endpoints = discovery::discover(http, &pc.issuer)?;
    let tokens = refresh::refresh(
        http,
        &endpoints.token_endpoint,
        &pc.client_id,
        pc.refresh_token.expose_secret(),
    )?;

    let updated = apply_tokens(pc, &tokens);
    let mut creds = storage::load(creds_path)?;
    if creds.session.is_some() {
        creds.session = Some(updated.clone());
        storage::save(creds_path, &creds)?;
    }
    Ok(updated)
}

/// Returns a [`ProfileCreds`] with the token fields replaced by `tokens`,
/// preserving the cached `issuer`/`client_id`/identity fields.
pub fn apply_tokens(pc: &ProfileCreds, tokens: &super::device::TokenSet) -> ProfileCreds {
    ProfileCreds::with_tokens(
        pc.api_url.clone(),
        pc.issuer.clone(),
        pc.client_id.clone(),
        pc.subject.clone(),
        pc.username.clone(),
        tokens,
    )
}
