//! On-disk OAuth credential cache: `~/.peppy/conf/credentials.json5`.
//!
//! json5 to match every other peppy config, written atomically with the file
//! chmodded `0600` and its parent `conf/` `0700` (secrets must not be
//! world-readable). Tokens are held as [`secrecy::SecretString`] so they never
//! surface in `Debug`/log output; they are serialized through an explicit
//! expose helper (the single intentional exposure point) and never logged.
//!
//! The data model holds a single session (`Credentials::session` is one
//! `Option`), so logging in against a different backend overwrites it rather
//! than adding a second entry. `issuer`/`client_id` are cached alongside the
//! tokens so a refresh does not need to re-hit `/cli/auth-config`.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// On-disk schema version of `credentials.json5`. Bumped on any shape change;
/// there is intentionally **no reader for an older version**. A clean break: a
/// pre-Phase-F file has no `version`, deserializes to `0`, and is rejected by
/// [`load`], so dev users simply re-run `peppy auth login` (acceptable pre-GA).
pub const CREDENTIALS_VERSION: u32 = 1;

/// Whole `credentials.json5` document: the schema version, a single cached OAuth
/// session, and the cached shared-router connection, or empty (just the
/// current version) when not logged in.
#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    /// Schema version (see [`CREDENTIALS_VERSION`]). Defaults to `0` when absent
    /// so an old/unversioned file is detected and rejected rather than
    /// half-interpreted.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub session: Option<ProfileCreds>,
    /// Cached per-user zenoh-router connection (from
    /// `POST /me/cli/messaging-federation`), or `None` until first fetched. Bound to `session`: cleared on login/logout so
    /// it can never outlive its identity.
    #[serde(default)]
    pub router: Option<RouterSession>,
}

impl Default for Credentials {
    /// An empty, not-logged-in document stamped with the current schema version,
    /// so a freshly-written file round-trips through [`load`]'s version check.
    fn default() -> Self {
        Self {
            version: CREDENTIALS_VERSION,
            session: None,
            router: None,
        }
    }
}

/// Cached per-user zenoh-router connection. Pulled from the backend after login
/// and reused until [`is_stale`](Self::is_stale). `Clone` is derivable (no
/// secrets; the capability lives in the endpoint and the link is end-to-end
/// TLS), unlike [`ProfileCreds`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterSession {
    /// The `<scheme>/<host>:<port>` locator the CLI dials.
    pub endpoint: String,
    /// Transport scheme echoed from the server (`"tls"` today); recorded so a
    /// future transport change is visible on disk.
    pub protocol: String,
    /// Absolute unix time after which the CLI re-resolves (and reconnects) on the
    /// next poke instead of reusing this cached config. A cache-freshness deadline
    /// only; derived at pull time from the server's `reconnect_after_secs`.
    pub repull_after: i64,
    /// The organization id this config was pulled for (the platform's stable
    /// per-user `Uuid`, as a string). Drives the daemon's session namespace, so
    /// it is cached alongside the endpoint. Required: a pre-`organization_id`
    /// file fails to parse with [`Error::Auth`], the intended clean break (the
    /// load-resilient `auth login`/`logout` then start fresh).
    pub organization_id: String,
    /// The OAuth subject the config was pulled for, tagging the cache to one
    /// identity. On reuse the daemon re-pulls when this no longer matches the
    /// active session, so a cache that survives an identity change can never be
    /// reused under the wrong org. Empty for a PAT pull (no session). Required
    /// for the same clean-break reason as `organization_id`.
    pub subject: String,
    /// The core-node name the config was pulled under (the pull's POST body,
    /// which registers the daemon in the backend's core-node registry). Tags
    /// the cache like `subject` does: a fresh cache pulled under a *different*
    /// name is not reused, so a renamed daemon (e.g. after a
    /// `CoreNodeNameTaken` collision fix) re-pulls — and re-registers — on its
    /// next resolve instead of staying absent from the registry until the
    /// cache goes stale. Required for the same clean-break reason as
    /// `organization_id`.
    pub core_node_name: String,
}

impl RouterSession {
    /// Whether the cached config is at/near its re-pull deadline, allowing `skew`
    /// seconds of slack so a slow re-resolve + handshake completes before the cache
    /// is treated as stale (mirrors [`ProfileCreds::is_expired`]).
    pub fn is_stale(&self, now_unix: i64, skew_secs: i64) -> bool {
        now_unix + skew_secs >= self.repull_after
    }
}

/// Cached credentials for one profile. The display-only `subject`/`username`
/// fields back `whoami` without a network round-trip.
///
/// `Clone` is hand-written (re-wrapping the secrets through [`secret`]) rather
/// than derived, so the struct does not depend on `SecretString: Clone`, which
/// varies across `secrecy` releases.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileCreds {
    pub api_url: String,
    pub issuer: String,
    pub client_id: String,
    #[serde(serialize_with = "expose", deserialize_with = "wrap")]
    pub access_token: SecretString,
    #[serde(serialize_with = "expose", deserialize_with = "wrap")]
    pub refresh_token: SecretString,
    /// Absolute expiry of `access_token`, unix seconds.
    pub expires_at: i64,
    pub token_type: String,
    pub scope: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub username: String,
}

impl ProfileCreds {
    /// Whether the access token is at or past expiry, allowing `skew` seconds of
    /// slack so we refresh slightly early rather than send a just-expired token.
    pub fn is_expired(&self, now_unix: i64, skew_secs: i64) -> bool {
        now_unix + skew_secs >= self.expires_at
    }

    /// Builds a [`ProfileCreds`] from the non-token identity fields plus a
    /// [`TokenSet`], centralizing the token-field mapping so `creds_from_login`
    /// and `apply_tokens` cannot drift.
    pub fn with_tokens(
        api_url: String,
        issuer: String,
        client_id: String,
        subject: String,
        username: String,
        tokens: &super::device::TokenSet,
    ) -> Self {
        Self {
            api_url,
            issuer,
            client_id,
            access_token: secret(tokens.access_token.clone()),
            refresh_token: secret(tokens.refresh_token.clone()),
            expires_at: tokens.expires_at,
            token_type: tokens.token_type.clone(),
            scope: tokens.scope.clone(),
            subject,
            username,
        }
    }
}

impl Clone for ProfileCreds {
    fn clone(&self) -> Self {
        Self {
            api_url: self.api_url.clone(),
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            access_token: secret(self.access_token.expose_secret().to_string()),
            refresh_token: secret(self.refresh_token.expose_secret().to_string()),
            expires_at: self.expires_at,
            token_type: self.token_type.clone(),
            scope: self.scope.clone(),
            subject: self.subject.clone(),
            username: self.username.clone(),
        }
    }
}

/// Builds a [`SecretString`] from an owned `String` without an extra copy.
pub fn secret(value: String) -> SecretString {
    SecretString::new(value.into_boxed_str())
}

fn expose<S: Serializer>(s: &SecretString, ser: S) -> std::result::Result<S::Ok, S::Error> {
    ser.serialize_str(s.expose_secret())
}

fn wrap<'de, D: Deserializer<'de>>(de: D) -> std::result::Result<SecretString, D::Error> {
    Ok(secret(String::deserialize(de)?))
}

/// Credentials path under a given peppy root: `<root>/conf/credentials.json5`.
/// Pairs with `peppy_config.json5` in the same `conf/` dir so a caller derives
/// both auth files from one [`PeppyDirs`].
pub fn credentials_path(dirs: &daemon_config::consts::PeppyDirs) -> PathBuf {
    dirs.conf_dir()
        .join(daemon_config::consts::CREDENTIALS_FILE)
}

/// Default credentials path: `<peppy root>/conf/credentials.json5`, honouring
/// `PEPPY_HOME`. The root is the global peppy data dir, never the cwd.
pub fn default_path() -> PathBuf {
    credentials_path(&daemon_config::consts::PeppyDirs::new(
        daemon_config::consts::peppy_root_dir(),
    ))
}

/// Loads the credentials document, returning an empty one when the file does
/// not exist yet (first login).
pub fn load(path: &Path) -> Result<Credentials> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let creds: Credentials = serde_json5::from_str(&content)
                .map_err(|e| Error::Auth(format!("failed to parse {}: {e}", path.display())))?;
            // No back-compat reader: any other version (including the
            // unversioned old format, which reads as 0) is rejected outright so
            // a stale-shaped file is never half-interpreted.
            if creds.version != CREDENTIALS_VERSION {
                return Err(Error::Auth(format!(
                    "credentials file {} is an unsupported format (v{}, expected v{}); \
                     run `peppy auth login` again",
                    path.display(),
                    creds.version,
                    CREDENTIALS_VERSION
                )));
            }
            Ok(creds)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Atomically writes the credentials document, setting `conf/` to `0700` and the
/// file to `0600` so the secrets are owner-only.
pub fn save(path: &Path, creds: &Credentials) -> Result<()> {
    let content = json5_pretty::to_string_pretty(creds)
        .map_err(|e| Error::Auth(format!("failed to serialize credentials: {e}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
    }

    daemon_config::atomic_write::publish_atomic(path, |tmp| {
        std::fs::write(tmp, &content)?;
        restrict_file(tmp)
    })?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Current time as unix seconds (0 if the clock predates the epoch).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Token values are deliberately not substrings of the field names so the
    // redaction test can't pass by accident.
    const ACCESS: &str = "zzz-access-9f3a";
    const REFRESH: &str = "yyy-refresh-71c2";

    fn sample() -> ProfileCreds {
        ProfileCreds {
            api_url: "http://127.0.0.1:3000".into(),
            issuer: "http://127.0.0.1:8080".into(),
            client_id: "cid".into(),
            access_token: secret(ACCESS.into()),
            refresh_token: secret(REFRESH.into()),
            expires_at: 1_000,
            token_type: "Bearer".into(),
            scope: "openid".into(),
            subject: "sub".into(),
            username: "alice".into(),
        }
    }

    #[test]
    fn round_trips_through_json5_and_keeps_tokens() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        let creds = Credentials {
            session: Some(sample()),
            ..Default::default()
        };

        save(&path, &creds).expect("save");
        let loaded = load(&path).expect("load");
        let pc = loaded.session.as_ref().expect("session");
        assert_eq!(pc.access_token.expose_secret(), ACCESS);
        assert_eq!(pc.refresh_token.expose_secret(), REFRESH);
        assert_eq!(pc.username, "alice");
    }

    #[test]
    fn round_trips_a_cached_router_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        let creds = Credentials {
            session: Some(sample()),
            router: Some(RouterSession {
                endpoint: "tls/cap.zenoh.localhost:7443".into(),
                protocol: "tls".into(),
                repull_after: 1_700_000_000,
                organization_id: "550e8400-e29b-41d4-a716-446655440000".into(),
                subject: "auth0|alice".into(),
                core_node_name: "core-node-alice-1".into(),
            }),
            ..Default::default()
        };

        save(&path, &creds).expect("save");
        let loaded = load(&path).expect("load");
        let rs = loaded.router.as_ref().expect("router session");
        assert_eq!(rs.endpoint, "tls/cap.zenoh.localhost:7443");
        assert_eq!(rs.protocol, "tls");
        assert_eq!(rs.repull_after, 1_700_000_000);
        assert_eq!(rs.organization_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(rs.subject, "auth0|alice");
        assert_eq!(rs.core_node_name, "core-node-alice-1");
    }

    /// A cached router session missing the `core_node_name` tag is rejected
    /// outright (no back-compat default), the same clean break as
    /// `organization_id`: `auth login`/`logout` start fresh.
    #[test]
    fn rejects_a_router_session_missing_the_core_node_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(
            &path,
            format!(
                r#"{{ version: {CREDENTIALS_VERSION}, router: {{
                    endpoint: "tls/cap:7443", protocol: "tls", repull_after: 1,
                    organization_id: "550e8400-e29b-41d4-a716-446655440000",
                    subject: "auth0|alice" }} }}"#
            ),
        )
        .expect("write pre-name-tag file");

        let err = load(&path).expect_err("missing name tag must be rejected");
        assert!(
            err.to_string().contains("failed to parse"),
            "rejection should surface as a parse error: {err}"
        );
    }

    #[test]
    fn default_document_carries_the_current_version() {
        assert_eq!(Credentials::default().version, CREDENTIALS_VERSION);
    }

    #[test]
    fn rejects_an_unversioned_old_format_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        // A pre-Phase-F document: no `version` field (reads as 0).
        std::fs::write(
            &path,
            r#"{ session: { api_url: "http://x", issuer: "http://y", client_id: "c",
                access_token: "a", refresh_token: "r", expires_at: 1, token_type: "Bearer",
                scope: "openid" } }"#,
        )
        .expect("write old file");

        let err = load(&path).expect_err("old format must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported format") && msg.contains("peppy auth login"),
            "rejection should be actionable: {msg}"
        );
    }

    #[test]
    fn router_session_staleness_accounts_for_skew() {
        let rs = RouterSession {
            endpoint: "tls/cap:7443".into(),
            protocol: "tls".into(),
            repull_after: 1_000,
            organization_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            subject: "auth0|alice".into(),
            core_node_name: "core-node-alice-1".into(),
        };
        assert!(!rs.is_stale(900, 30));
        assert!(rs.is_stale(980, 30)); // 980 + 30 >= 1000
        assert!(rs.is_stale(1_000, 0));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        save(&path, &Credentials::default()).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials must be owner-only");
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let creds = load(&dir.path().join("nope.json5")).expect("load missing");
        assert!(creds.session.is_none());
    }

    #[test]
    fn expiry_accounts_for_skew() {
        let pc = sample(); // expires_at = 1000
        assert!(!pc.is_expired(900, 30));
        assert!(pc.is_expired(980, 30)); // 980 + 30 >= 1000
        assert!(pc.is_expired(1000, 0));
    }

    #[test]
    fn debug_redacts_tokens() {
        let rendered = format!("{:?}", sample());
        assert!(
            !rendered.contains(ACCESS) && !rendered.contains(REFRESH),
            "token values must not appear in Debug output: {rendered}"
        );
    }
}
