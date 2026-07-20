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

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::{Error, Result};

/// On-disk schema version of `credentials.json5`. Bumped on any shape change;
/// [`load`] accepts only the current version.
pub const CREDENTIALS_VERSION: u32 = 1;

/// Whole `credentials.json5` document: the schema version, a single cached OAuth
/// session, and the cached shared-router connection, or empty (just the
/// current version) when not logged in.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// Schema version (see [`CREDENTIALS_VERSION`]). Defaults to `0` when absent
    /// so an old/unversioned file is detected and rejected rather than
    /// half-interpreted.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub session: Option<ProfileCreds>,
    /// Cached platform-router connection (from
    /// `POST /me/cli/federation`), or `None` until first fetched. Bound to `session`: cleared on login/logout so
    /// it can never outlive its identity.
    #[serde(default)]
    pub router: Option<RouterSession>,
    /// Non-secret metadata for the active production core-node certificate.
    /// Private key and certificate PEM bytes live only in the protected
    /// generation directory named by this record.
    #[serde(default)]
    pub core_node_identity: Option<crate::identity::CoreNodeIdentity>,
}

impl Default for Credentials {
    /// An empty, not-logged-in document stamped with the current schema version,
    /// so a freshly-written file round-trips through [`load`]'s version check.
    fn default() -> Self {
        Self {
            version: CREDENTIALS_VERSION,
            session: None,
            router: None,
            core_node_identity: None,
        }
    }
}

/// Cached platform-router connection. Pulled from the backend after login
/// and reused until [`is_stale`](Self::is_stale). `Clone` is derivable (no
/// secrets; the capability lives in the endpoint and the link is end-to-end
/// TLS), unlike [`ProfileCreds`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// The namespace this config was pulled for (the backend's `workspace_id`,
    /// already validated at the HTTP boundary). Drives the daemon's session
    /// namespace, so it is cached alongside the endpoint.
    pub namespace: config::namespace::Namespace,
    /// The OAuth subject the config was pulled for, tagging the cache to one
    /// identity. On reuse the daemon re-pulls when this no longer matches the
    /// active session, so a cache that survives an identity change can never be
    /// reused under the wrong workspace. Empty for a PAT pull (no session).
    /// Required for the same clean-break reason as `namespace`.
    pub subject: String,
    /// The core-node name the config was pulled under (the pull's POST body,
    /// which registers the daemon in the backend's core-node registry). Tags
    /// the cache like `subject` does: a fresh cache pulled under a *different*
    /// name is not reused, so a renamed daemon (e.g. after a
    /// `CoreNodeNameTaken` collision fix) re-pulls, and re-registers, on its
    /// next resolve instead of staying absent from the registry until the
    /// cache goes stale. Required for the same clean-break reason as
    /// `namespace`.
    pub core_node_name: String,
    /// Immutable identity generation used when this router config was pulled.
    /// A certificate rotation changes this value (and its PEM paths), forcing
    /// the managed router to reload even when the endpoint is unchanged.
    pub certificate_generation: String,
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
#[serde(deny_unknown_fields)]
pub struct ProfileCreds {
    /// Opaque identifier for this particular OAuth login. A fresh device login
    /// creates a new revision even when it resolves to the same subject; token
    /// refreshes preserve it. Delayed work from an earlier login is rejected
    /// when this revision no longer matches.
    pub session_revision: Uuid,
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
    pub subject: String,
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
        session_revision: Uuid,
        api_url: String,
        issuer: String,
        client_id: String,
        subject: String,
        username: String,
        tokens: &super::device::TokenSet,
    ) -> Self {
        Self {
            session_revision,
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
            session_revision: self.session_revision,
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
/// both auth files from one [`PeppyDirs`]. Every caller threads the `PeppyDirs`
/// it resolved at its own process boundary; there is deliberately no
/// default-root variant, so no auth read can silently reach the machine-global
/// peppy home.
pub fn credentials_path(dirs: &daemon_config::consts::PeppyDirs) -> PathBuf {
    dirs.conf_dir()
        .join(daemon_config::consts::CREDENTIALS_FILE)
}

/// Loads the credentials document, returning an empty one when the file does
/// not exist yet (first login).
pub fn load(path: &Path) -> Result<Credentials> {
    match read_private_credentials(path) {
        Ok(content) => parse_credentials(path, &content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Loads credentials for presentation without chmod-based repair. Unsafe
/// ownership, file type, or permissions are reported and left untouched.
pub fn load_read_only(path: &Path) -> Result<Credentials> {
    match read_private_credentials_read_only(path) {
        Ok(content) => parse_credentials(path, &content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn parse_credentials(path: &Path, content: &str) -> Result<Credentials> {
    #[derive(Deserialize)]
    struct VersionHeader {
        #[serde(default)]
        version: u32,
    }

    let header: VersionHeader = serde_json5::from_str(content)
        .map_err(|error| Error::Auth(format!("failed to parse {}: {error}", path.display())))?;
    if header.version != CREDENTIALS_VERSION {
        return Err(Error::Auth(format!(
            "credentials file {} is an unsupported format (v{}, expected v{}); refusing to modify it",
            path.display(),
            header.version,
            CREDENTIALS_VERSION
        )));
    }
    serde_json5::from_str(content)
        .map_err(|error| Error::Auth(format!("failed to parse {}: {error}", path.display())))
}

#[cfg(unix)]
fn read_private_credentials_read_only(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if let Some(parent) = path.parent()
        && parent.exists()
    {
        validate_private_dir_read_only(parent)?;
    }
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing symlink credentials path {}", path.display()),
        ));
    }
    if !path_metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-regular credentials path {}", path.display()),
        ));
    }
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "credentials file {} is not owned by the current user",
                path.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("credentials file {} is not mode 0600", path.display()),
        ));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

#[cfg(not(unix))]
fn read_private_credentials_read_only(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(unix)]
fn read_private_credentials(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if let Some(parent) = path.parent()
        && parent.exists()
    {
        restrict_dir(parent)?;
    }
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing symlink credentials path {}", path.display()),
        ));
    }
    if !path_metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-regular credentials path {}", path.display()),
        ));
    }
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "credentials file {} is not owned by the current user",
                path.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok(content)
}

#[cfg(not(unix))]
fn read_private_credentials(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Atomically writes a complete credentials document under the same stable
/// cross-process lock used by [`update`]. Production read/modify/write callers
/// should prefer `update`, so a stale snapshot cannot restore a session,
/// identity pointer, or router cache cleared by another process.
pub fn save(path: &Path, creds: &Credentials) -> Result<()> {
    let _lock = CredentialsLock::acquire(path)?;
    save_locked(path, creds)
}

/// Serializes a credentials read/modify/write transaction across CLI and
/// daemon processes. The callback sees the latest atomically-published v1
/// document and its targeted edits are published before the lock is released.
pub fn update<T>(path: &Path, mutate: impl FnOnce(&mut Credentials) -> Result<T>) -> Result<T> {
    let _lock = CredentialsLock::acquire(path)?;
    let mut creds = load(path)?;
    let result = mutate(&mut creds)?;
    save_locked(path, &creds)?;
    Ok(result)
}

/// Runs a read-only credentials check while holding the same stable lock as
/// every writer. Identity finalization uses this to make the session-revision
/// fence atomic with receipt commit: a fresh login cannot publish between the
/// final comparison and the durable identity decision.
pub(crate) fn inspect_locked<T>(
    path: &Path,
    inspect: impl FnOnce(&Credentials) -> Result<T>,
) -> Result<T> {
    let _lock = CredentialsLock::acquire(path)?;
    let creds = load(path)?;
    inspect(&creds)
}

/// Explicit destructive reset used only by `platform logout --offline` after
/// it has proven the daemon is stopped and acquired the lifetime identity-owner
/// lock. Unlike normal writers, this is allowed to replace a malformed or
/// unsupported document so orphaned renewable state can be removed.
pub fn reset_for_offline_recovery(path: &Path) -> Result<()> {
    let _lock = CredentialsLock::acquire(path)?;
    save_locked(path, &Credentials::default())
}

fn save_locked(path: &Path, creds: &Credentials) -> Result<()> {
    if creds.version != CREDENTIALS_VERSION {
        return Err(Error::Auth(format!(
            "refusing to write credentials format v{} (expected v{})",
            creds.version, CREDENTIALS_VERSION
        )));
    }
    let content = json5_pretty::to_string_pretty(creds)
        .map_err(|e| Error::Auth(format!("failed to serialize credentials: {e}")))?;
    let parent = path.parent().ok_or_else(|| {
        Error::Auth(format!(
            "credentials path {} has no parent directory",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    restrict_dir(parent)?;
    daemon_config::atomic_write::publish_atomic(path, |temporary| {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(temporary)?;
        file.write_all(content.as_bytes())?;
        restrict_file(temporary)?;
        file.sync_all()
    })?;
    #[cfg(test)]
    FAIL_AFTER_CREDENTIALS_RENAME.with(|fail| {
        if fail.replace(false) {
            return Err(Error::Io(std::io::Error::other(
                "injected failure after credentials rename",
            )));
        }
        Ok(())
    })?;
    // The file fsync makes its contents durable; the parent fsync makes the
    // atomic rename durable. A reported-success logout therefore cannot
    // resurrect the prior refresh/session document after power loss.
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_CREDENTIALS_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_credentials_parent_sync_after_rename() {
    FAIL_AFTER_CREDENTIALS_RENAME.with(|fail| fail.set(true));
}

/// A separate stable inode is required because publishing credentials uses an
/// atomic rename. Locking `credentials.json5` itself would leave concurrent
/// writers holding locks on different inodes after the first rename.
struct CredentialsLock {
    _file: File,
}

impl CredentialsLock {
    fn acquire(credentials_path: &Path) -> Result<Self> {
        let parent = credentials_path.parent().ok_or_else(|| {
            Error::Auth(format!(
                "credentials path {} has no parent directory",
                credentials_path.display()
            ))
        })?;
        let parent_existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
        if !parent_existed && let Some(grandparent) = parent.parent() {
            File::open(grandparent)?.sync_all()?;
        }
        let file_name = credentials_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials");
        let lock_path = parent.join(format!(".{file_name}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        restrict_file(&lock_path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn validate_private_dir_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = validate_owned_non_symlink(path)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory protected auth path {}",
                path.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "protected auth directory {} is not mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_dir_read_only(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory protected auth path {}",
                path.display()
            ),
        ))
    }
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = validate_owned_non_symlink(path)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory protected auth path {}",
                path.display()
            ),
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-directory protected auth path {}",
                path.display()
            ),
        ))
    }
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = validate_owned_non_symlink(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-regular protected auth path {}",
                path.display()
            ),
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn validate_owned_non_symlink(path: &Path) -> std::io::Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing symlink protected auth path {}", path.display()),
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "protected auth path {} is not owned by the current user",
                path.display()
            ),
        ));
    }
    Ok(metadata)
}

#[cfg(not(unix))]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-regular protected auth path {}",
                path.display()
            ),
        ))
    }
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
            session_revision: Uuid::parse_str("11111111-1111-4111-8111-111111111111")
                .expect("valid session revision"),
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
        assert_eq!(pc.session_revision, sample().session_revision);
    }

    #[test]
    fn concurrent_targeted_updates_preserve_session_and_identity_fields() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().expect("temp dir");
        let path = Arc::new(dir.path().join("conf").join("credentials.json5"));
        save(&path, &Credentials::default()).expect("seed");
        let barrier = Arc::new(Barrier::new(3));

        let session_path = Arc::clone(&path);
        let session_barrier = Arc::clone(&barrier);
        let session = std::thread::spawn(move || {
            session_barrier.wait();
            update(&session_path, |credentials| {
                credentials.session = Some(sample());
                Ok(())
            })
            .expect("session update");
        });

        let identity_path = Arc::clone(&path);
        let identity_barrier = Arc::clone(&barrier);
        let identity = std::thread::spawn(move || {
            identity_barrier.wait();
            update(&identity_path, |credentials| {
                credentials.core_node_identity = Some(crate::identity::CoreNodeIdentity {
                    api_origin: "https://api.peppy.bot".into(),
                    subject: "sub".into(),
                    session_revision: None,
                    workspace_id: config::namespace::Namespace::parse(
                        "550e8400-e29b-41d4-a716-446655440000",
                    )
                    .unwrap(),
                    core_node_name: "core-node-test".into(),
                    active_generation: "a".repeat(64),
                    serial_number: "01".into(),
                    spki_sha256: "a".repeat(64),
                    not_before: 1,
                    not_after: 3,
                    renew_after: 2,
                });
                Ok(())
            })
            .expect("identity update");
        });

        barrier.wait();
        session.join().unwrap();
        identity.join().unwrap();
        let final_state = load(&path).expect("final credentials");
        assert!(final_state.session.is_some());
        assert!(final_state.core_node_identity.is_some());
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
                namespace: config::namespace::Namespace::parse(
                    "550e8400-e29b-41d4-a716-446655440000",
                )
                .expect("valid test namespace"),
                subject: "auth0|alice".into(),
                core_node_name: "core-node-alice-1".into(),
                certificate_generation: "debug-shared-v1".into(),
            }),
            ..Default::default()
        };

        save(&path, &creds).expect("save");
        let loaded = load(&path).expect("load");
        let rs = loaded.router.as_ref().expect("router session");
        assert_eq!(rs.endpoint, "tls/cap.zenoh.localhost:7443");
        assert_eq!(rs.protocol, "tls");
        assert_eq!(rs.repull_after, 1_700_000_000);
        assert_eq!(
            rs.namespace.as_str(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(rs.subject, "auth0|alice");
        assert_eq!(rs.core_node_name, "core-node-alice-1");
        assert_eq!(rs.certificate_generation, "debug-shared-v1");
    }

    /// A cached router session must carry the core-node name it was pulled for.
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
                    namespace: "550e8400-e29b-41d4-a716-446655440000",
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
        assert!(Credentials::default().session.is_none());
    }

    #[test]
    fn rejects_old_and_future_credentials_versions_without_overwriting_them() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        for version in [0, CREDENTIALS_VERSION + 1, 3, 99] {
            let original = format!(r#"{{ version: {version}, marker: "preserve-me" }}"#);
            std::fs::write(&path, &original).expect("write mismatched file");

            let err = load(&path).expect_err("mismatched version must be rejected");
            assert!(err.to_string().contains("unsupported format"), "{err}");
            update(&path, |_| Ok(())).expect_err("a writer must not heal an unsupported version");
            assert_eq!(
                std::fs::read_to_string(&path).expect("read preserved file"),
                original
            );
        }
    }

    #[test]
    fn malformed_credentials_fail_closed_without_being_overwritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let original = "{ this is not valid JSON5";
        std::fs::write(&path, original).expect("write malformed file");

        update(&path, |_| Ok(())).expect_err("malformed input must be rejected");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn explicit_offline_recovery_can_reset_malformed_credentials() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, "{ malformed").expect("write malformed file");

        reset_for_offline_recovery(&path).expect("explicit recovery reset");

        let reset = load(&path).unwrap();
        assert_eq!(reset.version, CREDENTIALS_VERSION);
        assert!(reset.session.is_none());
        assert!(reset.router.is_none());
        assert!(reset.core_node_identity.is_none());
    }

    #[test]
    fn version_one_session_without_a_revision_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let original = r#"{
            version: 1,
            session: {
                api_url: "https://api.example",
                issuer: "https://issuer.example",
                client_id: "cli-client",
                access_token: "access",
                refresh_token: "refresh",
                expires_at: 1000,
                token_type: "Bearer",
                scope: "openid"
            }
        }"#;
        std::fs::write(&path, original).expect("write revision-less session");

        load(&path).expect_err("a session revision is mandatory in the v1 schema");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn version_one_session_without_display_identity_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        let mut value = serde_json::to_value(Credentials {
            session: Some(sample()),
            ..Default::default()
        })
        .expect("serialize credentials");
        value["session"].as_object_mut().unwrap().remove("subject");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).expect("write credentials");

        load(&path).expect_err("the v1 session shape must not accept legacy display fields");
    }

    #[test]
    fn version_one_rejects_unknown_top_level_and_nested_fields() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("conf").join("credentials.json5");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");

        std::fs::write(&path, r#"{ version: 1, legacy_field: true }"#)
            .expect("write unknown top-level field");
        load(&path).expect_err("unknown top-level fields must fail closed");

        let mut value = serde_json::to_value(Credentials {
            session: Some(sample()),
            ..Default::default()
        })
        .expect("serialize credentials");
        value["session"]["legacy_field"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .expect("write unknown nested field");
        load(&path).expect_err("unknown nested fields must fail closed");
    }

    #[test]
    fn router_session_staleness_accounts_for_skew() {
        let rs = RouterSession {
            endpoint: "tls/cap:7443".into(),
            protocol: "tls".into(),
            repull_after: 1_000,
            namespace: config::namespace::Namespace::parse("550e8400-e29b-41d4-a716-446655440000")
                .expect("valid test namespace"),
            subject: "auth0|alice".into(),
            core_node_name: "core-node-alice-1".into(),
            certificate_generation: "debug-shared-v1".into(),
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

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        load(&path).expect("an owned credentials file is safely re-restricted on load");
        let repaired = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(repaired & 0o777, 0o600);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let creds = load(&dir.path().join("nope.json5")).expect("load missing");
        assert!(creds.session.is_none());
    }

    #[test]
    fn credentials_loader_rejects_a_non_regular_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("credentials.json5");
        std::fs::create_dir(&path).unwrap();

        let error = load(&path).expect_err("a directory must never be read as protected JSON");
        assert!(error.to_string().contains("non-regular"), "{error}");
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
