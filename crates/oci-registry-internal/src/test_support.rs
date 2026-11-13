//! A local CNCF distribution registry for the tests that need a real one.
//!
//! The binary is the one `PEPPY_TEST_REGISTRY_BIN` names: CI downloads a
//! pinned release of `registry` from github.com/distribution/distribution.
//! Each [`LocalRegistry`] has its own storage directory and loopback port,
//! and stops when it is dropped.

use std::io::BufRead as _;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// The environment variable that names the `registry` binary.
pub const REGISTRY_BIN_ENV: &str = "PEPPY_TEST_REGISTRY_BIN";

/// The user of a registry started with basic auth.
pub const TEST_USERNAME: &str = "peppy";

/// The password of [`TEST_USERNAME`].
pub const TEST_PASSWORD: &str = "peppy-registry-test";

/// The bcrypt hash of [`TEST_PASSWORD`], as the htpasswd file of the
/// registry holds it.
const TEST_PASSWORD_BCRYPT: &str = "$2b$04$mjqZbbtZGPOpthGEEzOKOOmMrnhNbF4T3/BqXwfSwF7jHXLe4fupm";

/// How many times a start is tried on a new port when the port a start
/// picked was taken by another process in the meantime.
const START_ATTEMPTS: usize = 5;

/// The prefix of the variables the registry reads as its configuration.
const REGISTRY_CONFIG_ENV_PREFIX: &str = "REGISTRY_";

/// Whether the registry asks for a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryAuth {
    /// Every request is accepted without a credential.
    Anonymous,
    /// Every request needs [`TEST_USERNAME`] and [`TEST_PASSWORD`], with
    /// HTTP basic authentication.
    Basic,
}

/// A running registry. Dropping it stops the process.
pub struct LocalRegistry {
    child: Child,
    address: SocketAddr,
    dir: tempfile::TempDir,
}

impl LocalRegistry {
    /// Starts a registry with `auth`, and returns once it listens.
    ///
    /// # Panics
    ///
    /// When `PEPPY_TEST_REGISTRY_BIN` is unset, or the registry does not
    /// start.
    pub fn start(auth: RegistryAuth) -> Self {
        let binary = std::env::var_os(REGISTRY_BIN_ENV).unwrap_or_else(|| {
            panic!(
                "{REGISTRY_BIN_ENV} must name the `registry` binary of \
                 github.com/distribution/distribution (v3) for the registry tests"
            )
        });
        let mut last_error = String::new();
        for _ in 0..START_ATTEMPTS {
            match Self::try_start(Path::new(&binary), auth) {
                Ok(registry) => return registry,
                Err(error) => last_error = error,
            }
        }
        panic!("the local registry did not start: {last_error}");
    }

    fn try_start(binary: &Path, auth: RegistryAuth) -> Result<Self, String> {
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let address = free_loopback_address()?;
        let storage = dir.path().join("storage");
        let mut config = format!(
            "version: 0.1\n\
             log:\n  level: info\n  formatter: text\n\
             storage:\n  filesystem:\n    rootdirectory: {}\n  delete:\n    enabled: true\n\
             http:\n  addr: {address}\n",
            storage.display()
        );
        if auth == RegistryAuth::Basic {
            let htpasswd = dir.path().join("htpasswd");
            std::fs::write(
                &htpasswd,
                format!("{TEST_USERNAME}:{TEST_PASSWORD_BCRYPT}\n"),
            )
            .map_err(|e| format!("htpasswd: {e}"))?;
            config.push_str(&format!(
                "auth:\n  htpasswd:\n    realm: peppy-test\n    path: {}\n",
                htpasswd.display()
            ));
        }
        let config_path = dir.path().join("config.yml");
        std::fs::write(&config_path, config).map_err(|e| format!("config: {e}"))?;
        let mut command = Command::new(binary);
        // The registry reads each `REGISTRY_<KEY>` variable of its
        // environment as the value of that key of its configuration, so a
        // variable of the caller with this prefix would change the
        // configuration above.
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .starts_with(REGISTRY_CONFIG_ENV_PREFIX)
            {
                command.env_remove(name);
            }
        }
        let mut child = command
            .arg("serve")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", binary.display()))?;
        let stderr = child.stderr.take().expect("stderr is piped");
        // The registry logs one line when it listens. Its stderr closing
        // first means it stopped: the port was taken, or the config is bad.
        let mut lines = std::io::BufReader::new(stderr).lines();
        let mut seen = Vec::new();
        let listening = loop {
            match lines.next() {
                Some(Ok(line)) => {
                    let found = line.contains("listening on");
                    seen.push(line);
                    if found {
                        break true;
                    }
                }
                Some(Err(_)) | None => break false,
            }
        };
        if !listening {
            let _ = child.kill();
            let _ = child.wait();
            return Err(seen.join("\n"));
        }
        // Keep draining its log so the registry never blocks on a full pipe.
        std::thread::spawn(move || for _ in lines {});
        Ok(Self {
            child,
            address,
            dir,
        })
    }

    /// The directory the registry stores its blobs and manifests in, with
    /// the layout of the filesystem driver of distribution.
    pub fn storage_dir(&self) -> PathBuf {
        self.dir.path().join("storage")
    }

    /// `127.0.0.1:<port>`.
    pub fn registry(&self) -> String {
        self.address.to_string()
    }

    /// `oci://127.0.0.1:<port>/<path>`.
    pub fn repository_url(&self, path: &str) -> String {
        format!("oci://{}/{path}", self.address)
    }

    /// Writes a Docker style auth file at `path` with the test credential of
    /// this registry.
    pub fn write_auth_file(&self, path: &Path) {
        use base64::Engine as _;
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{TEST_USERNAME}:{TEST_PASSWORD}"));
        std::fs::write(
            path,
            format!(
                r#"{{"auths": {{"{}": {{"auth": "{auth}"}}}}}}"#,
                self.address
            ),
        )
        .expect("write the registry auth file");
    }
}

impl Drop for LocalRegistry {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A loopback address whose port no process listens on now.
fn free_loopback_address() -> Result<SocketAddr, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    listener.local_addr().map_err(|e| format!("address: {e}"))
}

/// The directory of a stub credential helper `docker-credential-<name>`
/// that answers `username` and `password` for every registry. Put the
/// directory first in the `PATH` the credential store searches.
pub fn write_stub_credential_helper(
    dir: &Path,
    name: &str,
    username: &str,
    password: &str,
) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("credential-helpers");
    std::fs::create_dir_all(&bin).expect("create the helper directory");
    let helper = bin.join(format!("docker-credential-{name}"));
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\n[ \"$1\" = get ] || exit 2\nread registry\n\
             printf '{{\"ServerURL\":\"%s\",\"Username\":\"{username}\",\"Secret\":\"{password}\"}}' \"$registry\"\n"
        ),
    )
    .expect("write the stub helper");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub helper executable");
    bin
}
