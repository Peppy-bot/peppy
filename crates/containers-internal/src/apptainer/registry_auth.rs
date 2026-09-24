//! Registry credentials handed to apptainer on the native backend.
//!
//! Apptainer reads registry credentials from exactly one Docker-style auth
//! file: the one `APPTAINER_AUTH_FILE` (the env spelling of `--authfile`)
//! names, else its own `docker-config.json` (written by `apptainer registry
//! login`) when that exists, else Docker's `config.json`. It never consults
//! `DOCKER_CONFIG`.
//!
//! Docker CLI's web login (`docker login` without `-u`) stores three entries in
//! that file: the Docker Hub personal access token under
//! `https://index.docker.io/v1/`, plus its own OAuth access and refresh tokens
//! under `https://index.docker.io/v1/access-token` and
//! `https://index.docker.io/v1/refresh-token`. Apptainer fetches `Bootstrap:
//! docker` images with the containers/image library, which matches all three
//! keys to Docker Hub and takes whichever one Go's randomized map iteration
//! yields first. A build therefore randomly presents the expired access token
//! or the refresh token, and Docker Hub answers "incorrect username or
//! password", even for a public image that needs no credential at all.
//!
//! So on the native backend apptainer never reads the user's file directly.
//! Before each command that can reach a registry, peppy copies the file
//! apptainer would read into a peppy-owned file without those two OAuth
//! entries, and points apptainer at the copy through `APPTAINER_AUTH_FILE`.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Env spelling of apptainer's `--authfile` flag. An explicit `--authfile` in a
/// node's `apptainer_build_extra_args` still takes precedence over it.
pub(crate) const APPTAINER_AUTH_FILE_ENV: &str = "APPTAINER_AUTH_FILE";

/// Env override of apptainer's configuration directory (`~/.apptainer`).
const APPTAINER_CONFIGDIR_ENV: &str = "APPTAINER_CONFIGDIR";

/// Keys under which Docker CLI's web login keeps its OAuth tokens (see
/// `internal/oauth/manager` in docker/cli): a short-lived access token and a
/// refresh token suffixed with the OAuth client id. Neither is a registry
/// password. The credential that login produces for Docker Hub is the
/// personal access token it stores under `https://index.docker.io/v1/`.
const DOCKER_CLI_OAUTH_TOKEN_KEYS: [&str; 2] = [
    "https://index.docker.io/v1/access-token",
    "https://index.docker.io/v1/refresh-token",
];

/// File mode of the sanitized copy: it holds the same secrets as its source.
const SANITIZED_FILE_MODE: u32 = 0o600;

/// Mode of the directory that holds the sanitized copy.
const SANITIZED_DIR_MODE: u32 = 0o700;

/// The auth file apptainer reads when nothing points it elsewhere, located the
/// way apptainer 1.5.2 locates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryAuthSource {
    /// `APPTAINER_AUTH_FILE` names the file.
    Named(PathBuf),
    /// Apptainer's own `docker-config.json` when it exists, else Docker's
    /// `config.json`. Which one applies is decided when a command is
    /// assembled, as apptainer decides it on each run.
    ApptainerThenDocker {
        apptainer_config: PathBuf,
        docker_config: PathBuf,
    },
}

impl RegistryAuthSource {
    /// Locates the source from this process's environment.
    pub(crate) fn from_env() -> Result<Self> {
        Self::from_env_with(|key| std::env::var_os(key), dirs::home_dir())
    }

    /// [`Self::from_env`] with the environment and home directory made
    /// explicit, so the precedence is testable without mutating process env.
    ///
    /// An `APPTAINER_CONFIGDIR` override holds both candidate files, including
    /// Docker's `config.json`: apptainer resolves its `.docker` fallback
    /// through the same override.
    fn from_env_with(
        env: impl Fn(&str) -> Option<OsString>,
        home: Option<PathBuf>,
    ) -> Result<Self> {
        if let Some(auth_file) =
            daemon_config::consts::non_empty_env_path(env(APPTAINER_AUTH_FILE_ENV))
        {
            return Ok(Self::Named(auth_file));
        }
        if let Some(config_dir) =
            daemon_config::consts::non_empty_env_path(env(APPTAINER_CONFIGDIR_ENV))
        {
            return Ok(Self::ApptainerThenDocker {
                apptainer_config: config_dir.join("docker-config.json"),
                docker_config: config_dir.join("config.json"),
            });
        }
        let home = home.ok_or_else(|| {
            Error::ConfigurationError(format!(
                "cannot locate apptainer's registry auth file: no home directory is known \
                 and {APPTAINER_CONFIGDIR_ENV} is unset"
            ))
        })?;
        Ok(Self::ApptainerThenDocker {
            apptainer_config: home.join(".apptainer/docker-config.json"),
            docker_config: home.join(".docker/config.json"),
        })
    }

    /// The file apptainer would read right now. Only a confirmed absence of
    /// apptainer's own file falls back to Docker's, as in apptainer: any other
    /// stat failure keeps it, so the read reports the real problem.
    fn current_path(&self) -> &Path {
        match self {
            Self::Named(path) => path,
            Self::ApptainerThenDocker {
                apptainer_config,
                docker_config,
            } => match fs::metadata(apptainer_config) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => docker_config,
                _ => apptainer_config,
            },
        }
    }
}

/// Where the native backend reads registry credentials from, and where it
/// writes the sanitized copy apptainer is pointed at.
#[derive(Debug)]
pub(crate) struct RegistryAuth {
    pub(crate) source: RegistryAuthSource,
    pub(crate) sanitized_file: PathBuf,
}

impl RegistryAuth {
    /// The source located from this process's environment, with the sanitized
    /// copy under the peppy tree's temporary directory.
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            source: RegistryAuthSource::from_env()?,
            sanitized_file: daemon_config::consts::PeppyDirs::default()
                .tmp_dir()
                .join("registry-auth")
                .join("docker-config.json"),
        })
    }

    /// Rewrites the sanitized copy from the source's current contents and
    /// returns its path.
    ///
    /// Runs before every command that can reach a registry, so a `docker
    /// login` or `docker logout` made while the daemon runs applies to the
    /// next build. A missing source yields a copy with no credentials (every
    /// pull is anonymous, as apptainer would do), which also clears the
    /// secrets an earlier copy held.
    pub(crate) fn prepare(&self) -> Result<&Path> {
        let source = self.source.current_path();
        let credentials = read_registry_credentials(source)?;
        if !credentials.dropped_keys.is_empty() {
            tracing::debug!(
                source = %source.display(),
                dropped = ?credentials.dropped_keys,
                "left Docker CLI OAuth token entries out of the registry auth file handed to apptainer"
            );
        }
        let contents = serde_json::to_vec_pretty(&credentials.file)
            .expect("serializing string-keyed JSON maps cannot fail");
        write_private_file(&self.sanitized_file, &contents).map_err(|source| {
            Error::SanitizedRegistryAuthUnwritable {
                path: self.sanitized_file.display().to_string(),
                source,
            }
        })?;
        Ok(&self.sanitized_file)
    }
}

/// A Docker-style auth file reduced to the fields apptainer reads credentials
/// from. Entries under `auths` stay opaque: peppy only decides which keys to
/// keep, never what an entry holds.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
struct AuthFileCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auths: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(
        rename = "credHelpers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    cred_helpers: Option<BTreeMap<String, String>>,
    #[serde(
        rename = "credsStore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    creds_store: Option<String>,
}

/// An auth file parsed for apptainer: its credential fields without Docker
/// CLI's OAuth token entries, and the keys that were left out.
#[derive(Debug, PartialEq)]
struct SanitizedCredentials {
    file: AuthFileCredentials,
    dropped_keys: Vec<String>,
}

impl SanitizedCredentials {
    /// Parses the contents of a Docker-style auth file. An empty (or
    /// whitespace-only) file holds no credentials, as Docker CLI reads it.
    fn parse(contents: &[u8]) -> serde_json::Result<Self> {
        if contents.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self {
                file: AuthFileCredentials::default(),
                dropped_keys: Vec::new(),
            });
        }
        let mut file: AuthFileCredentials = serde_json::from_slice(contents)?;
        let mut dropped_keys = Vec::new();
        if let Some(auths) = &mut file.auths {
            for key in DOCKER_CLI_OAUTH_TOKEN_KEYS {
                if auths.remove(key).is_some() {
                    dropped_keys.push(key.to_string());
                }
            }
        }
        Ok(Self { file, dropped_keys })
    }
}

/// Reads and sanitizes `path`. A missing file holds no credentials.
fn read_registry_credentials(path: &Path) -> Result<SanitizedCredentials> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(Error::RegistryAuthFileUnreadable {
                path: path.display().to_string(),
                source,
            });
        }
    };
    SanitizedCredentials::parse(&contents).map_err(|source| Error::RegistryAuthFileInvalid {
        path: path.display().to_string(),
        source,
    })
}

/// Replaces `path` with `contents`, readable by this user only.
///
/// The parent directory is created if needed and restricted to this user. A
/// symlink in its place is refused rather than followed, and a directory
/// owned by someone else fails the permission change: the file holds
/// secrets, so it must never land where another user can read it. The new
/// contents are written to a sibling file created with the private mode and
/// renamed over `path`, so a concurrent build never reads a partial file and
/// the secrets are never readable under a wider mode.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a directory", dir.display()),
        ));
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(SANITIZED_DIR_MODE))?;

    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let mut staging_name = file_name.to_os_string();
    staging_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let staging = dir.join(staging_name);

    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SANITIZED_FILE_MODE)
        .open(&staging)
        .and_then(|mut file| file.write_all(contents))
        .and_then(|()| fs::rename(&staging, path));
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A scratch directory under the shared test root.
    fn scratch() -> TempDir {
        TempDir::new_in(config_test_support::test_tmp_root()).expect("create scratch dir")
    }

    /// An env lookup that only knows `vars`.
    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        move |key| {
            vars.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).expect("read sanitized file"))
            .expect("sanitized file holds JSON")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    /// The config a Docker CLI web login leaves behind, next to a second
    /// registry, a credential helper and settings that carry no credentials.
    const WEB_LOGIN_CONFIG: &str = r#"{
        "auths": {
            "https://index.docker.io/v1/": {"auth": "cGVwcHk6ZGNrcl9wYXRfdmFsaWQ="},
            "https://index.docker.io/v1/access-token": {"auth": "cGVwcHk6ZXhwaXJlZC1qd3Q="},
            "https://index.docker.io/v1/refresh-token": {"auth": "cGVwcHk6cmVmcmVzaC4uY2xpZW50"},
            "ghcr.io": {"auth": "Z2hjcjp0b2tlbg=="}
        },
        "credHelpers": {"123456789012.dkr.ecr.us-east-1.amazonaws.com": "ecr-login"},
        "credsStore": "secretservice",
        "psFormat": "table {{.ID}}",
        "currentContext": "default"
    }"#;

    fn web_login_expected() -> serde_json::Value {
        serde_json::json!({
            "auths": {
                "https://index.docker.io/v1/": {"auth": "cGVwcHk6ZGNrcl9wYXRfdmFsaWQ="},
                "ghcr.io": {"auth": "Z2hjcjp0b2tlbg=="}
            },
            "credHelpers": {"123456789012.dkr.ecr.us-east-1.amazonaws.com": "ecr-login"},
            "credsStore": "secretservice"
        })
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_leaves_out_the_docker_cli_oauth_tokens_and_keeps_every_credential() {
        let parsed = SanitizedCredentials::parse(WEB_LOGIN_CONFIG.as_bytes()).expect("parse");

        assert_eq!(
            serde_json::to_value(&parsed.file).expect("serialize"),
            web_login_expected()
        );
        assert_eq!(
            parsed.dropped_keys,
            vec![
                "https://index.docker.io/v1/access-token".to_string(),
                "https://index.docker.io/v1/refresh-token".to_string(),
            ]
        );
    }

    /// A classic `docker login -u` config has nothing to leave out.
    #[test]
    fn parse_keeps_a_classic_login_config_whole() {
        let config =
            r#"{"auths": {"https://index.docker.io/v1/": {"auth": "cGVwcHk6ZGNrcl9wYXQ="}}}"#;

        let parsed = SanitizedCredentials::parse(config.as_bytes()).expect("parse");

        assert!(parsed.dropped_keys.is_empty());
        assert_eq!(
            serde_json::to_value(&parsed.file).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(config).expect("fixture is JSON")
        );
    }

    #[test]
    fn parse_reads_an_empty_file_as_no_credentials() {
        for contents in ["", "  \n\t"] {
            let parsed = SanitizedCredentials::parse(contents.as_bytes()).expect("parse");
            assert_eq!(
                parsed.file,
                AuthFileCredentials::default(),
                "contents {contents:?}"
            );
            assert!(parsed.dropped_keys.is_empty());
        }
    }

    /// `null` is how Go marshals a nil map, so it means "no entries", as in
    /// both Docker CLI and containers/image.
    #[test]
    fn parse_reads_null_fields_as_absent() {
        let config = r#"{"auths": null, "credHelpers": null, "credsStore": null}"#;

        let parsed = SanitizedCredentials::parse(config.as_bytes()).expect("parse");

        assert_eq!(parsed.file, AuthFileCredentials::default());
        assert_eq!(serde_json::to_vec(&parsed.file).expect("serialize"), b"{}");
    }

    #[test]
    fn parse_rejects_what_apptainer_could_not_read_either() {
        for contents in [
            "{not json",
            r#"{"auths": []}"#,
            r#"{"credHelpers": {"ghcr.io": 7}}"#,
            r#"{"credsStore": {}}"#,
        ] {
            assert!(
                SanitizedCredentials::parse(contents.as_bytes()).is_err(),
                "contents {contents:?} should be rejected"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Source location
    // -----------------------------------------------------------------------

    #[test]
    fn source_is_the_named_auth_file_when_apptainer_auth_file_is_set() {
        let source = RegistryAuthSource::from_env_with(
            env_of(&[
                ("APPTAINER_AUTH_FILE", "/secrets/auth.json"),
                ("APPTAINER_CONFIGDIR", "/cfg"),
            ]),
            Some(PathBuf::from("/home/u")),
        )
        .expect("locate source");

        assert_eq!(
            source,
            RegistryAuthSource::Named(PathBuf::from("/secrets/auth.json"))
        );
    }

    #[test]
    fn source_searches_the_home_directory_by_default() {
        let source = RegistryAuthSource::from_env_with(env_of(&[]), Some(PathBuf::from("/home/u")))
            .expect("locate source");

        assert_eq!(
            source,
            RegistryAuthSource::ApptainerThenDocker {
                apptainer_config: PathBuf::from("/home/u/.apptainer/docker-config.json"),
                docker_config: PathBuf::from("/home/u/.docker/config.json"),
            }
        );
    }

    #[test]
    fn source_searches_the_apptainer_configdir_override_for_both_files() {
        let source = RegistryAuthSource::from_env_with(
            env_of(&[("APPTAINER_CONFIGDIR", "/cfg")]),
            Some(PathBuf::from("/home/u")),
        )
        .expect("locate source");

        assert_eq!(
            source,
            RegistryAuthSource::ApptainerThenDocker {
                apptainer_config: PathBuf::from("/cfg/docker-config.json"),
                docker_config: PathBuf::from("/cfg/config.json"),
            }
        );
    }

    /// An empty value counts as unset, as it does for apptainer.
    #[test]
    fn source_ignores_empty_overrides() {
        let source = RegistryAuthSource::from_env_with(
            env_of(&[("APPTAINER_AUTH_FILE", ""), ("APPTAINER_CONFIGDIR", "")]),
            Some(PathBuf::from("/home/u")),
        )
        .expect("locate source");

        assert_eq!(
            source,
            RegistryAuthSource::ApptainerThenDocker {
                apptainer_config: PathBuf::from("/home/u/.apptainer/docker-config.json"),
                docker_config: PathBuf::from("/home/u/.docker/config.json"),
            }
        );
    }

    #[test]
    fn source_needs_a_home_directory_without_an_override() {
        let err = RegistryAuthSource::from_env_with(env_of(&[]), None)
            .expect_err("no home and no override cannot be located");

        assert!(
            matches!(&err, Error::ConfigurationError(msg) if msg.contains("APPTAINER_CONFIGDIR")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn current_path_prefers_apptainers_own_file_only_while_it_exists() {
        let dir = scratch();
        let source = RegistryAuthSource::ApptainerThenDocker {
            apptainer_config: dir.path().join("docker-config.json"),
            docker_config: dir.path().join("config.json"),
        };

        assert_eq!(source.current_path(), dir.path().join("config.json"));

        fs::write(dir.path().join("docker-config.json"), "{}").expect("write apptainer file");
        assert_eq!(source.current_path(), dir.path().join("docker-config.json"));
    }

    // -----------------------------------------------------------------------
    // Preparing the sanitized copy
    // -----------------------------------------------------------------------

    fn registry_auth_in(dir: &Path) -> RegistryAuth {
        RegistryAuth {
            source: RegistryAuthSource::ApptainerThenDocker {
                apptainer_config: dir.join("home/.apptainer/docker-config.json"),
                docker_config: dir.join("home/.docker/config.json"),
            },
            sanitized_file: dir.join("peppy/registry-auth/docker-config.json"),
        }
    }

    fn write_docker_config(dir: &Path, contents: &str) {
        let path = dir.join("home/.docker/config.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("create .docker");
        fs::write(path, contents).expect("write docker config");
    }

    #[test]
    fn prepare_writes_a_private_sanitized_copy_and_leaves_the_source_alone() {
        let dir = scratch();
        write_docker_config(dir.path(), WEB_LOGIN_CONFIG);
        let registry_auth = registry_auth_in(dir.path());

        let sanitized = registry_auth.prepare().expect("prepare");

        assert_eq!(sanitized, registry_auth.sanitized_file);
        assert_eq!(read_json(sanitized), web_login_expected());
        assert_eq!(mode_of(sanitized), SANITIZED_FILE_MODE);
        assert_eq!(
            mode_of(sanitized.parent().expect("parent")),
            SANITIZED_DIR_MODE
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("home/.docker/config.json")).expect("read source"),
            WEB_LOGIN_CONFIG,
            "the user's own file must never be modified"
        );
        let leftovers: Vec<_> = fs::read_dir(sanitized.parent().expect("parent"))
            .expect("list dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, vec![OsString::from("docker-config.json")]);
    }

    #[test]
    fn prepare_reads_apptainers_own_file_over_dockers() {
        let dir = scratch();
        write_docker_config(
            dir.path(),
            r#"{"auths": {"docker.example": {"auth": "ZDpk"}}}"#,
        );
        let apptainer_config = dir.path().join("home/.apptainer/docker-config.json");
        fs::create_dir_all(apptainer_config.parent().expect("parent")).expect("create .apptainer");
        fs::write(
            &apptainer_config,
            r#"{"auths": {"apptainer.example": {"auth": "YTph"}}}"#,
        )
        .expect("write apptainer config");

        let sanitized = registry_auth_in(dir.path())
            .prepare()
            .expect("prepare")
            .to_path_buf();

        assert_eq!(
            read_json(&sanitized),
            serde_json::json!({"auths": {"apptainer.example": {"auth": "YTph"}}})
        );
    }

    /// Logging out between two builds must not leave the earlier secrets in
    /// peppy's copy.
    #[test]
    fn prepare_without_a_source_writes_an_empty_copy_over_an_earlier_one() {
        let dir = scratch();
        write_docker_config(dir.path(), WEB_LOGIN_CONFIG);
        let registry_auth = registry_auth_in(dir.path());
        registry_auth.prepare().expect("first prepare");

        fs::remove_file(dir.path().join("home/.docker/config.json")).expect("log out");
        let sanitized = registry_auth.prepare().expect("second prepare");

        assert_eq!(read_json(sanitized), serde_json::json!({}));
    }

    #[test]
    fn prepare_restricts_an_existing_directory_to_this_user() {
        let dir = scratch();
        let registry_auth = registry_auth_in(dir.path());
        let sanitized_dir = registry_auth.sanitized_file.parent().expect("parent");
        fs::create_dir_all(sanitized_dir).expect("pre-create dir");
        fs::set_permissions(sanitized_dir, fs::Permissions::from_mode(0o755)).expect("chmod");

        registry_auth.prepare().expect("prepare");

        assert_eq!(mode_of(sanitized_dir), SANITIZED_DIR_MODE);
    }

    #[test]
    fn prepare_refuses_a_symlink_in_place_of_its_directory() {
        let dir = scratch();
        let registry_auth = registry_auth_in(dir.path());
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("create target");
        let sanitized_dir = registry_auth.sanitized_file.parent().expect("parent");
        fs::create_dir_all(sanitized_dir.parent().expect("grandparent")).expect("create parent");
        std::os::unix::fs::symlink(&elsewhere, sanitized_dir).expect("plant symlink");

        let err = registry_auth
            .prepare()
            .expect_err("a symlinked directory must be refused");

        assert!(
            matches!(err, Error::SanitizedRegistryAuthUnwritable { .. }),
            "unexpected error: {err}"
        );
        assert_eq!(
            fs::read_dir(&elsewhere).expect("list target").count(),
            0,
            "nothing may be written through the symlink"
        );
    }

    #[test]
    fn prepare_names_an_invalid_source() {
        let dir = scratch();
        write_docker_config(dir.path(), "{not json");

        let err = registry_auth_in(dir.path())
            .prepare()
            .expect_err("invalid JSON");

        match err {
            Error::RegistryAuthFileInvalid { path, .. } => {
                assert!(path.ends_with("home/.docker/config.json"), "path {path}")
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn prepare_names_an_unreadable_source() {
        let dir = scratch();
        // A directory where the file should be: reading it fails with an error
        // other than NotFound whatever the privileges of the test runner.
        fs::create_dir_all(dir.path().join("home/.docker/config.json")).expect("create dir");

        let err = registry_auth_in(dir.path())
            .prepare()
            .expect_err("unreadable source");

        match err {
            Error::RegistryAuthFileUnreadable { path, .. } => {
                assert!(path.ends_with("home/.docker/config.json"), "path {path}")
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
