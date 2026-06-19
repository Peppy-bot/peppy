//! Credential resolution for authenticated commands.
//!
//! Precedence (the interactive device-login step is owned by the `login`
//! command, not the resolver — `whoami`/`logout` never auto-open a browser):
//!
//! 1. `PEPPY_API_KEY` PAT → bearer, no refresh (CI / automation).
//! 2. cached token for the active profile, valid → use it.
//! 3. cached token expired but refreshable → refresh, persist rotation, use it.
//! 4. otherwise → [`Error::NotAuthenticated`].

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

use super::storage::{self, ProfileCreds};
use super::{discovery, profile::Profile, refresh};
use crate::error::{Error, Result};

/// Refresh slightly before the real expiry to avoid racing a just-expired token.
const EXPIRY_SKEW_SECS: i64 = 30;

/// A ready bearer plus the context needed to refresh it on a reactive `401`.
pub struct Credential {
    pub token: SecretString,
    pub kind: CredentialKind,
    pub profile: String,
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

/// Resolves a usable credential for `profile`. `pat` is the injected
/// `PEPPY_API_KEY` value (production passes the env var; tests pass it
/// explicitly to avoid env races).
pub fn resolve(
    profile: &Profile,
    creds_path: &Path,
    agent: &ureq::Agent,
    pat: Option<String>,
) -> Result<Credential> {
    if let Some(pat) = pat.filter(|v| !v.is_empty()) {
        return Ok(Credential {
            token: storage::secret(pat),
            kind: CredentialKind::Pat,
            profile: profile.name.clone(),
        });
    }

    let mut creds = storage::load(creds_path)?;
    let pc = creds
        .profiles
        .get(&profile.name)
        .cloned()
        .ok_or(Error::NotAuthenticated)?;

    if !pc.is_expired(storage::now_unix(), EXPIRY_SKEW_SECS) {
        return Ok(session_credential(profile, creds_path, &pc));
    }

    // Expired: refresh proactively and persist the rotation.
    let endpoints = discovery::discover(agent, &pc.issuer)?;
    let tokens = refresh::refresh(
        agent,
        &endpoints.token_endpoint,
        &pc.client_id,
        pc.refresh_token.expose_secret(),
    )
    .map_err(|e| {
        Error::Auth(format!(
            "{e}\nYour session may have expired — run `peppy login`."
        ))
    })?;

    let updated = apply_tokens(&pc, &tokens);
    creds.profiles.insert(profile.name.clone(), updated.clone());
    storage::save(creds_path, &creds)?;
    Ok(session_credential(profile, creds_path, &updated))
}

/// Returns a [`ProfileCreds`] with the token fields replaced by `tokens`,
/// preserving the cached `issuer`/`client_id`/identity fields.
pub fn apply_tokens(pc: &ProfileCreds, tokens: &super::device::TokenSet) -> ProfileCreds {
    ProfileCreds {
        api_url: pc.api_url.clone(),
        issuer: pc.issuer.clone(),
        client_id: pc.client_id.clone(),
        access_token: storage::secret(tokens.access_token.clone()),
        refresh_token: storage::secret(tokens.refresh_token.clone()),
        expires_at: tokens.expires_at,
        token_type: tokens.token_type.clone(),
        scope: tokens.scope.clone(),
        subject: pc.subject.clone(),
        username: pc.username.clone(),
    }
}

fn session_credential(profile: &Profile, creds_path: &Path, pc: &ProfileCreds) -> Credential {
    Credential {
        token: storage::secret(pc.access_token.expose_secret().to_string()),
        kind: CredentialKind::Session(SessionContext {
            issuer: pc.issuer.clone(),
            client_id: pc.client_id.clone(),
            refresh_token: storage::secret(pc.refresh_token.expose_secret().to_string()),
            creds_path: creds_path.to_path_buf(),
        }),
        profile: profile.name.clone(),
    }
}
