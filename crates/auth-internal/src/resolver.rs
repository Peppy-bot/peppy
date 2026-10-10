//! Credential resolution for authenticated commands.
//!
//! The interactive device-login step is owned by the `login` command, not the
//! resolver (`whoami`/`enroll` never auto-open a browser). Resolution is:
//!
//! 1. cached session token, valid → use it.
//! 2. cached session token expired but refreshable → refresh, persist rotation,
//!    use it.
//! 3. otherwise → [`Error::NotAuthenticated`].

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

use super::http::HttpClient;
use super::storage::{self, ProfileCreds};
use super::{discovery, profile, refresh};
use crate::error::{Error, Result};

/// Refresh slightly before the real expiry to avoid racing a just-expired token.
const EXPIRY_SKEW_SECS: i64 = 30;

/// A ready bearer plus the context needed to refresh it on a reactive `401`.
pub struct Credential {
    pub token: SecretString,
    pub session: SessionContext,
}

/// Everything needed to refresh a session token and persist the rotation.
pub struct SessionContext {
    pub issuer: String,
    pub client_id: String,
    pub refresh_token: SecretString,
    pub creds_path: PathBuf,
}

impl SessionContext {
    /// Whether `stored` is the session this context was built from, maybe
    /// with tokens that another command rotated since: a session of the same
    /// issuer and client. A session of another issuer or client comes from a
    /// login at another platform, so its bearer does not belong where the
    /// bearer of this context was sent.
    pub(crate) fn is_same_session_as(&self, stored: &ProfileCreds) -> bool {
        self.issuer == stored.issuer && self.client_id == stored.client_id
    }
}

/// Resolves a usable credential from the single cached session.
pub fn resolve(creds_path: &Path, http: &HttpClient) -> Result<Credential> {
    let Some(pc) = storage::load(creds_path)?.session else {
        return Err(Error::NotAuthenticated);
    };
    resolve_session(creds_path, http, &pc)
}

/// Resolves a usable credential from `pc`, the session a caller already read
/// from `creds_path`, so the bearer belongs to the session the caller checked.
pub(crate) fn resolve_session(
    creds_path: &Path,
    http: &HttpClient,
    pc: &ProfileCreds,
) -> Result<Credential> {
    if !pc.is_expired(storage::now_unix(), EXPIRY_SKEW_SECS) {
        return Ok(session_credential(creds_path, pc));
    }

    // Expired: refresh proactively and persist the rotation.
    let updated = refresh_and_persist(http, creds_path, pc).map_err(|e| {
        Error::Auth(format!(
            "{e}\nYour session may have expired, run `peppy platform login`."
        ))
    })?;
    Ok(session_credential(creds_path, &updated))
}

/// Builds a refreshable session [`Credential`] from the cached `pc`. Public
/// for the one caller outside the resolver: `peppy platform login`, which builds
/// the credential from tokens it minted seconds ago instead of re-resolving.
pub fn session_credential(creds_path: &Path, pc: &ProfileCreds) -> Credential {
    Credential {
        token: storage::secret(pc.access_token.expose_secret().to_string()),
        session: SessionContext {
            issuer: pc.issuer.clone(),
            client_id: pc.client_id.clone(),
            refresh_token: storage::secret(pc.refresh_token.expose_secret().to_string()),
            creds_path: creds_path.to_path_buf(),
        },
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
    let endpoints = discovery::discover(http, &pc.issuer, profile::build_transport_policy())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Credentials;

    /// An unexpired session of `issuer` for the client `cli`, whose bearer is
    /// `access`. It never expires, so no test of it depends on the clock.
    fn session(issuer: &str, access: &str) -> ProfileCreds {
        ProfileCreds {
            api_url: issuer.to_string(),
            issuer: issuer.to_string(),
            client_id: "cli".to_string(),
            access_token: storage::secret(access.to_string()),
            refresh_token: storage::secret(format!("{access}-refresh")),
            expires_at: i64::MAX,
            token_type: "Bearer".to_string(),
            scope: "openid".to_string(),
            subject: "sub".to_string(),
            username: "alice".to_string(),
        }
    }

    /// A login at another platform can replace the session in the file after
    /// the caller read and checked its own: the bearer stays the checked one.
    #[test]
    fn resolving_a_session_takes_its_bearer_and_not_the_one_in_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        let checked = session("https://api.a.test", "access-a");
        let in_the_file = Credentials {
            session: Some(session("https://api.b.test", "access-b")),
            ..Default::default()
        };
        storage::save(&path, &in_the_file).expect("save");

        let credential =
            resolve_session(&path, &HttpClient::new(), &checked).expect("an unexpired session");

        assert_eq!(credential.token.expose_secret(), "access-a");
        assert_eq!(credential.session.issuer, "https://api.a.test");
        assert_eq!(
            credential.session.refresh_token.expose_secret(),
            "access-a-refresh"
        );
    }

    #[test]
    fn a_stored_session_is_the_same_session_only_with_the_same_issuer_and_client() {
        let context =
            session_credential(Path::new("creds"), &session("https://a.test", "a")).session;
        let rotated = ProfileCreds {
            expires_at: 1,
            ..session("https://a.test", "rotated")
        };
        let other_client = ProfileCreds {
            client_id: "other-cli".to_string(),
            ..session("https://a.test", "a")
        };
        let cases = [
            ("rotated tokens", rotated, true),
            ("another issuer", session("https://b.test", "a"), false),
            ("another client", other_client, false),
        ];
        for (case, stored, is_same) in cases {
            assert_eq!(context.is_same_session_as(&stored), is_same, "{case}");
        }
    }
}
