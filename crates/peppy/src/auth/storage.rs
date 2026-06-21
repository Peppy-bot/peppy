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
//! tokens so a refresh does not need to re-hit `/cli-config`.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};

/// Whole `credentials.json5` document: a single cached session, or empty when
/// not logged in.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub session: Option<ProfileCreds>,
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
pub fn credentials_path(dirs: &config::consts::PeppyDirs) -> PathBuf {
    dirs.conf_dir().join(config::consts::CREDENTIALS_FILE)
}

/// Default credentials path: `<peppy root>/conf/credentials.json5`, honouring
/// `PEPPY_HOME`. The root is the global peppy data dir, never the cwd.
pub fn default_path() -> PathBuf {
    credentials_path(&config::consts::PeppyDirs::new(
        config::consts::peppy_root_dir(),
    ))
}

/// Loads the credentials document, returning an empty one when the file does
/// not exist yet (first login).
pub fn load(path: &Path) -> Result<Credentials> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json5::from_str(&content)
            .map_err(|e| Error::Auth(format!("failed to parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Atomically writes the credentials document, setting `conf/` to `0700` and the
/// file to `0600` so the secrets are owner-only.
pub fn save(path: &Path, creds: &Credentials) -> Result<()> {
    let content = config::json5_pretty::to_string_pretty(creds)
        .map_err(|e| Error::Auth(format!("failed to serialize credentials: {e}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
    }

    config::atomic_write::publish_atomic(path, |tmp| {
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
        };

        save(&path, &creds).expect("save");
        let loaded = load(&path).expect("load");
        let pc = loaded.session.as_ref().expect("session");
        assert_eq!(pc.access_token.expose_secret(), ACCESS);
        assert_eq!(pc.refresh_token.expose_secret(), REFRESH);
        assert_eq!(pc.username, "alice");
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
