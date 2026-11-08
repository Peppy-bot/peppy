//! Registry credentials: where peppy finds them, and how a registry asks for
//! them.
//!
//! Peppy reads the Docker style auth file that apptainer reads
//! ([`containers::RegistryAuthFile`]), so `docker login` and `apptainer
//! registry login` both work. For a registry, the credential comes from, in
//! this order: the credential helper `credHelpers` names for it, the helper
//! `credsStore` names, then its entry under `auths`. Without any, peppy asks
//! the registry without a credential, which a public repository accepts for
//! a pull.

use crate::error::{RegistryError, Result};
use base64::Engine as _;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt as _;

/// A credential for one registry.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A user name and a password or an access token, which a registry takes
    /// with HTTP basic authentication, or exchanges for a bearer token.
    Basic { username: String, password: String },
    /// An OAuth 2 refresh token, which the token service of the registry
    /// exchanges for a bearer token (`identitytoken` in the auth file).
    IdentityToken(String),
}

impl std::fmt::Debug for Credential {
    // The secret stays out of every log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => write!(f, "Basic({username}, <secret>)"),
            Self::IdentityToken(_) => f.write_str("IdentityToken(<secret>)"),
        }
    }
}

impl Credential {
    /// The value of an `Authorization: Basic` header.
    pub(crate) fn basic_header(&self) -> Option<String> {
        match self {
            Self::Basic { username, password } => Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            )),
            Self::IdentityToken(_) => None,
        }
    }
}

/// Where the auth file is.
#[derive(Debug, Clone)]
enum AuthFileLocation {
    /// Where apptainer finds it, from the environment of the process at the
    /// time of each read.
    Environment,
    /// This file.
    Path(PathBuf),
}

/// Where peppy finds registry credentials: the auth file, and the
/// directories that the credential helpers it names are searched in.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    auth_file: AuthFileLocation,
    /// The `PATH` that `docker-credential-<helper>` is looked up in. `None`
    /// is the `PATH` of this process.
    helper_search_path: Option<OsString>,
}

impl CredentialStore {
    /// The auth file apptainer reads, and the helpers on the `PATH` of this
    /// process. The file is located again at each read, as apptainer does.
    pub fn from_environment() -> Self {
        Self {
            auth_file: AuthFileLocation::Environment,
            helper_search_path: None,
        }
    }

    /// The auth file at `auth_file`, and the helpers in the directories of
    /// `helper_search_path` (a `PATH` value).
    pub fn new(auth_file: PathBuf, helper_search_path: OsString) -> Self {
        Self {
            auth_file: AuthFileLocation::Path(auth_file),
            helper_search_path: Some(helper_search_path),
        }
    }

    /// The credential for `registry` (`host` or `host:port`), or `None` when
    /// the auth file names none.
    pub async fn credential_for(&self, registry: &str) -> Result<Option<Credential>> {
        let file = match &self.auth_file {
            AuthFileLocation::Environment => containers::RegistryAuthFile::read_from_env()?,
            AuthFileLocation::Path(path) => containers::RegistryAuthFile::read(path)?,
        };
        if let Some(helper) = registry_aliases(registry)
            .iter()
            .find_map(|alias| file.credential_helper(alias))
        {
            return self.run_helper(helper, registry).await;
        }
        if let Some(helper) = file.credentials_store()
            && let Some(credential) = self.run_helper(helper, registry).await?
        {
            return Ok(Some(credential));
        }
        let aliases = registry_aliases(registry);
        for (key, entry) in file.auths() {
            if aliases.contains(&auth_key_registry(key).as_str()) {
                return credential_of_entry(registry, entry);
            }
        }
        Ok(None)
    }

    /// Runs `docker-credential-<helper> get` for `registry`. A helper that
    /// has no credential for the registry answers `None`.
    async fn run_helper(&self, helper: &str, registry: &str) -> Result<Option<Credential>> {
        let program = format!("docker-credential-{helper}");
        let mut command = tokio::process::Command::new(&program);
        command
            .arg("get")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = &self.helper_search_path {
            // A command name without `/` is looked up in the `PATH` of the
            // environment the child gets.
            command.env("PATH", path);
        }
        let failure = |detail: String| RegistryError::CredentialHelper {
            helper: helper.to_owned(),
            detail,
        };
        let mut child = command
            .spawn()
            .map_err(|e| failure(format!("cannot start {program}: {e}")))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        // A helper that fails before it reads the registry closes its stdin
        // first. Its exit status and its stderr then say why it failed, so
        // the broken pipe of the write is not the error to report.
        match stdin.write_all(registry.as_bytes()).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(error) => {
                return Err(failure(format!("cannot write the registry to it: {error}")));
            }
        }
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| failure(format!("cannot read its answer: {e}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // The message every helper of docker/docker-credential-helpers
            // gives for a registry it has no credential for.
            if stdout.contains("credentials not found") || stderr.contains("credentials not found")
            {
                return Ok(None);
            }
            return Err(failure(format!(
                "it exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        let answer: HelperAnswer = serde_json::from_str(&stdout)
            .map_err(|e| failure(format!("its answer is not the expected JSON: {e}")))?;
        if answer.secret.is_empty() {
            return Ok(None);
        }
        // Docker's convention: a helper that holds an identity token answers
        // it as the secret of the user `<token>`.
        if answer.username == "<token>" {
            return Ok(Some(Credential::IdentityToken(answer.secret)));
        }
        Ok(Some(Credential::Basic {
            username: answer.username,
            password: answer.secret,
        }))
    }
}

/// What `docker-credential-<helper> get` prints.
#[derive(serde::Deserialize)]
struct HelperAnswer {
    #[serde(rename = "Username", default)]
    username: String,
    #[serde(rename = "Secret", default)]
    secret: String,
}

/// The credential an `auths` entry holds: `identitytoken`, else `auth`
/// (base64 of `user:password`), else `username` and `password`.
fn credential_of_entry(registry: &str, entry: &serde_json::Value) -> Result<Option<Credential>> {
    let field = |name: &str| {
        entry
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    if let Some(token) = field("identitytoken") {
        return Ok(Some(Credential::IdentityToken(token.to_owned())));
    }
    if let Some(encoded) = field("auth") {
        let invalid = || RegistryError::Unauthorized {
            registry: registry.to_owned(),
            detail: "the `auth` of its entry in the registry auth file is not the base64 of \
                     `user:password`"
                .to_owned(),
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| invalid())?;
        let decoded = String::from_utf8(decoded).map_err(|_| invalid())?;
        let (username, password) = decoded.split_once(':').ok_or_else(invalid)?;
        return Ok(Some(Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }));
    }
    if let (Some(username), Some(password)) = (field("username"), field("password")) {
        return Ok(Some(Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }));
    }
    Ok(None)
}

/// The registry an `auths` key names: the key without its scheme and path,
/// lowercase. `https://ghcr.io`, `ghcr.io` and `ghcr.io/v2/` all name
/// `ghcr.io`.
fn auth_key_registry(key: &str) -> String {
    let without_scheme = key.split_once("://").map_or(key, |(_, rest)| rest).trim();
    without_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The hosts of Docker Hub. The auth file keeps the Docker Hub credential
/// under `https://index.docker.io/v1/`, while its API is at
/// `registry-1.docker.io` and users name it `docker.io`.
const DOCKER_HUB_HOSTS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];

/// The names the auth file may know `registry` by.
fn registry_aliases(registry: &str) -> Vec<&str> {
    if DOCKER_HUB_HOSTS.contains(&registry) {
        DOCKER_HUB_HOSTS.to_vec()
    } else {
        vec![registry]
    }
}

/// The host that serves the API of `registry`: Docker Hub serves it at
/// `registry-1.docker.io`, every other registry at its own host.
pub(crate) fn api_host(registry: &str) -> &str {
    if DOCKER_HUB_HOSTS.contains(&registry) {
        "registry-1.docker.io"
    } else {
        registry
    }
}

/// How a registry asks for a credential: the `WWW-Authenticate` header of a
/// `401` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Challenge {
    /// HTTP basic authentication on every request.
    Basic,
    /// A bearer token from the token service at `realm`.
    Bearer { params: BTreeMap<String, String> },
}

impl Challenge {
    /// Parses `Basic realm="..."` or `Bearer realm="...",service="...",
    /// scope="..."`. Parameter names are case insensitive, values may be
    /// quoted and quoted values may hold commas.
    pub(crate) fn parse(header: &str) -> Option<Self> {
        let header = header.trim();
        let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
        if scheme.eq_ignore_ascii_case("basic") {
            return Some(Self::Basic);
        }
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let mut params = BTreeMap::new();
        let mut chars = rest.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
                chars.next();
            }
            let name: String = chars
                .by_ref()
                .take_while(|c| *c != '=')
                .collect::<String>()
                .trim()
                .to_ascii_lowercase();
            if name.is_empty() {
                break;
            }
            let mut value = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => value.extend(chars.next()),
                        '"' => break,
                        _ => value.push(c),
                    }
                }
            } else {
                while let Some(c) = chars.peek() {
                    if *c == ',' {
                        break;
                    }
                    value.push(*c);
                    chars.next();
                }
                value = value.trim().to_owned();
            }
            params.insert(name, value);
        }
        params
            .contains_key("realm")
            .then_some(Self::Bearer { params })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(
        auth_file: &str,
        helpers: &[(&str, &str)],
    ) -> (tempfile::TempDir, CredentialStore) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, auth_file).expect("write auth file");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin dir");
        for (name, script) in helpers {
            let helper = bin.join(format!("docker-credential-{name}"));
            std::fs::write(&helper, format!("#!/bin/sh\n{script}\n")).expect("write helper");
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
                .expect("chmod helper");
        }
        let search_path =
            std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
                .expect("join paths");
        (dir, CredentialStore::new(path, search_path))
    }

    fn basic(username: &str, password: &str) -> Credential {
        Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    #[tokio::test]
    async fn an_auths_entry_is_found_under_any_spelling_of_its_registry() {
        // base64 of `peppy:s3cret`.
        for key in [
            "ghcr.io",
            "https://ghcr.io",
            "https://GHCR.io/v2/",
            "http://ghcr.io",
        ] {
            let file = format!(r#"{{"auths": {{"{key}": {{"auth": "cGVwcHk6czNjcmV0"}}}}}}"#);
            let (_dir, store) = store_with(&file, &[]);
            assert_eq!(
                store.credential_for("ghcr.io").await.expect("read"),
                Some(basic("peppy", "s3cret")),
                "{key}"
            );
            assert_eq!(store.credential_for("quay.io").await.expect("read"), None);
        }
    }

    #[tokio::test]
    async fn the_docker_hub_entry_serves_every_docker_hub_host() {
        let file = r#"{"auths": {"https://index.docker.io/v1/": {"auth": "cGVwcHk6czNjcmV0"}}}"#;
        let (_dir, store) = store_with(file, &[]);
        for registry in DOCKER_HUB_HOSTS {
            assert_eq!(
                store.credential_for(registry).await.expect("read"),
                Some(basic("peppy", "s3cret")),
                "{registry}"
            );
        }
        assert_eq!(api_host("docker.io"), "registry-1.docker.io");
        assert_eq!(api_host("ghcr.io"), "ghcr.io");
    }

    #[tokio::test]
    async fn an_identity_token_and_a_username_password_entry_are_read() {
        let (_dir, store) = store_with(
            r#"{"auths": {
                "a.example": {"identitytoken": "refresh-me", "auth": "cGVwcHk6czNjcmV0"},
                "b.example": {"username": "u", "password": "p"},
                "c.example": {"email": "nobody@example.com"}
            }}"#,
            &[],
        );
        assert_eq!(
            store.credential_for("a.example").await.expect("read"),
            Some(Credential::IdentityToken("refresh-me".to_owned()))
        );
        assert_eq!(
            store.credential_for("b.example").await.expect("read"),
            Some(basic("u", "p"))
        );
        assert_eq!(store.credential_for("c.example").await.expect("read"), None);
    }

    #[tokio::test]
    async fn an_auth_that_is_not_base64_of_user_and_password_is_refused() {
        let (_dir, store) = store_with(r#"{"auths": {"ghcr.io": {"auth": "bm8tY29sb24="}}}"#, &[]);
        let error = store.credential_for("ghcr.io").await.expect_err("no colon");
        assert!(error.to_string().contains("base64"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_auth_file_names_no_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path().join("absent.json"), OsString::new());
        assert_eq!(store.credential_for("ghcr.io").await.expect("read"), None);
    }

    #[tokio::test]
    async fn a_credential_helper_for_the_registry_wins_over_the_auths_entry() {
        let (_dir, store) = store_with(
            r#"{
                "auths": {"ghcr.io": {"auth": "cGVwcHk6czNjcmV0"}},
                "credHelpers": {"ghcr.io": "stub"}
            }"#,
            &[(
                "stub",
                r#"read registry; [ "$1" = get ] || exit 3; printf '{"ServerURL":"%s","Username":"helper","Secret":"from-%s"}' "$registry" "$registry""#,
            )],
        );
        assert_eq!(
            store.credential_for("ghcr.io").await.expect("read"),
            Some(basic("helper", "from-ghcr.io"))
        );
    }

    #[tokio::test]
    async fn a_credentials_store_without_the_registry_falls_back_to_the_auths_entry() {
        let (_dir, store) = store_with(
            r#"{
                "auths": {"ghcr.io": {"auth": "cGVwcHk6czNjcmV0"}},
                "credsStore": "stub"
            }"#,
            &[(
                "stub",
                r#"read registry; if [ "$registry" = quay.io ]; then echo '{"Username":"<token>","Secret":"tok"}'; else echo "credentials not found in native keychain"; exit 1; fi"#,
            )],
        );
        assert_eq!(
            store.credential_for("ghcr.io").await.expect("read"),
            Some(basic("peppy", "s3cret"))
        );
        assert_eq!(
            store.credential_for("quay.io").await.expect("read"),
            Some(Credential::IdentityToken("tok".to_owned()))
        );
    }

    /// The broken helper exits without reading the registry, so the write
    /// of the registry may meet a closed pipe: the error still gives the
    /// exit status and the stderr of the helper.
    #[tokio::test]
    async fn a_failing_or_missing_credential_helper_is_an_error_that_names_it() {
        let (_dir, store) = store_with(
            r#"{"credHelpers": {"ghcr.io": "broken", "quay.io": "absent"}}"#,
            &[("broken", "echo 'keychain locked' >&2; exit 2")],
        );
        let error = store.credential_for("ghcr.io").await.expect_err("broken");
        let message = error.to_string();
        assert!(message.contains("docker-credential-broken"), "{message}");
        assert!(message.contains("keychain locked"), "{message}");
        let error = store.credential_for("quay.io").await.expect_err("absent");
        assert!(
            error.to_string().contains("docker-credential-absent"),
            "{error}"
        );
    }

    #[test]
    fn the_debug_form_of_a_credential_hides_its_secret() {
        let shown = format!(
            "{:?} {:?}",
            basic("u", "hunter2"),
            Credential::IdentityToken("tok".into())
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("tok)"),
            "{shown}"
        );
    }

    #[test]
    fn challenges_parse_with_quoted_values_that_hold_commas() {
        assert_eq!(
            Challenge::parse(r#"Basic realm="Registry Realm""#),
            Some(Challenge::Basic)
        );
        let challenge = Challenge::parse(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:acme/nodes:pull,push""#,
        )
        .expect("bearer");
        let Challenge::Bearer { params } = challenge else {
            panic!("bearer expected");
        };
        assert_eq!(params["realm"], "https://ghcr.io/token");
        assert_eq!(params["service"], "ghcr.io");
        assert_eq!(params["scope"], "repository:acme/nodes:pull,push");

        let unquoted =
            Challenge::parse("bearer Realm=https://auth.example/token, service=x").expect("bearer");
        let Challenge::Bearer { params } = unquoted else {
            panic!("bearer expected");
        };
        assert_eq!(params["realm"], "https://auth.example/token");
        assert_eq!(params["service"], "x");

        assert_eq!(Challenge::parse(r#"Bearer service="x""#), None);
        assert_eq!(Challenge::parse("Negotiate abc"), None);
    }
}
