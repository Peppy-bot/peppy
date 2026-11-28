//! Generation of the zenohd router's own config file. This is the daemon's
//! deployment config (how to run the router), distinct from the client session
//! configs that live with the messaging adapter. Both are produced by the
//! shared [`crate::zenoh_config`] builder.

use super::ZenohNetProtocol;
use crate::error::{Error, Result};
use crate::router_id::RouterId;
use crate::zenoh_config::{TlsConfig, render_config_string, router_spec};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The config file that a managed zenohd runs.
#[derive(Debug)]
pub(crate) enum RouterConfigFile {
    /// A file that peppy does not own and leaves in place: the operator's
    /// `ZENOH_CONFIG`, which the router runs as it is.
    Unowned(PathBuf),
    /// A config that peppy rendered.
    Rendered(RenderedRouterConfig),
}

impl RouterConfigFile {
    /// The path of the file.
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Unowned(path) => path,
            Self::Rendered(rendered) => &rendered.path,
        }
    }
}

/// A router config that peppy rendered into a new directory of its own,
/// which only the user of the daemon can open (mode `0700`). Another user of
/// the machine can neither read the config, nor put a file at its path before
/// the daemon writes it, nor change it after. The file name holds the port
/// (`zenohd_config_<port>.json5`), so the command line of the router names
/// its port.
///
/// The process that rendered the config removes the directory when it drops
/// this value. A fork of that process leaves the directory to it: the router
/// that the owner restarts reads the config again.
#[derive(Debug)]
pub(crate) struct RenderedRouterConfig {
    dir: tempfile::TempDir,
    path: PathBuf,
    owner_pid: u32,
}

impl RenderedRouterConfig {
    /// Writes `content` to `zenohd_config_<messaging_port>.json5` in a new
    /// directory of `parent` that only the user of this process can open.
    fn write(parent: &Path, messaging_port: u16, content: &str) -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("peppy-zenohd-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let path = dir
            .path()
            .join(format!("zenohd_config_{messaging_port}.json5"));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(content.as_bytes())?;
        Ok(Self {
            dir,
            path,
            owner_pid: std::process::id(),
        })
    }
}

impl Drop for RenderedRouterConfig {
    fn drop(&mut self) {
        if std::process::id() != self.owner_pid {
            self.dir.disable_cleanup(true);
        }
    }
}

/// Resolves the zenohd router config. Honors a `ZENOH_CONFIG` override;
/// otherwise renders a router config into a private directory of the
/// temporary directory (see [`RenderedRouterConfig`]). `tls` (when the
/// protocol is `Tls`, or when a `tls/` `connect_endpoint` is present) carries
/// the listener's certificate/key and/or the connect-side trust root and
/// client identity; `None` renders a plaintext listener. `connect_endpoints`
/// federates this router to those upstream routers (empty for a standalone
/// router); see [`crate::zenoh_config::router_spec`].
///
/// `router_id` pins the spawned router's transport identity. It is ignored on
/// the `ZENOH_CONFIG` path, where the operator owns the whole config including
/// whatever `id` it does or does not carry.
pub(crate) fn router_config_file(
    protocol: ZenohNetProtocol,
    host: &str,
    messaging_port: u16,
    connect_endpoints: Vec<String>,
    tls: Option<TlsConfig>,
    router_id: &RouterId,
) -> Result<RouterConfigFile> {
    if let Some(config_path) = config_override() {
        return Ok(RouterConfigFile::Unowned(config_path));
    }

    // The router seeds gossip discovery for the peer mesh, so gossip stays on;
    // multicast is off everywhere (see `crate::zenoh_config`). The router listens
    // on `host` as given. Shares `router_spec` with the out-of-process render
    // path (`render_router_config`).
    let config_content = render_config_string(&router_spec(
        protocol,
        host,
        messaging_port,
        true,
        connect_endpoints,
        tls,
        router_id,
    ));

    RenderedRouterConfig::write(&std::env::temp_dir(), messaging_port, &config_content)
        .map(RouterConfigFile::Rendered)
        .map_err(|e| Error::ConfigurationError(format!("Failed to write zenohd config: {e}")))
}

/// The operator-pinned router config path from `ZENOH_CONFIG`, if set. When
/// present, [`router_config_file`] returns it untouched: peppy never renders its
/// own config over an operator-owned one, and
/// [`ZenohdFacade::managed`](super::ZenohdFacade::managed) records the pin so
/// the daemon can report it instead of pretending to own the router's
/// federation. This is the single source of truth for the override so the two
/// call sites cannot drift.
pub(crate) fn config_override() -> Option<PathBuf> {
    // A blank or whitespace-only `ZENOH_CONFIG` is treated as unset (not an empty
    // path), so startup falls back to the rendered temp config instead of trying
    // to read `""`.
    std::env::var("ZENOH_CONFIG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_rendered_config_is_in_a_new_directory_that_only_its_user_opens() {
        let parent = tempfile::tempdir().expect("temp dir");

        let rendered = RenderedRouterConfig::write(parent.path(), 7448, "{ mode: \"router\" }")
            .expect("write");

        assert_eq!(
            rendered.path.file_name().and_then(|name| name.to_str()),
            Some("zenohd_config_7448.json5")
        );
        let dir = rendered
            .path
            .parent()
            .expect("the config is in a directory");
        assert_eq!(dir.parent(), Some(parent.path()));
        assert_eq!(mode_of(dir), 0o700);
        assert_eq!(mode_of(&rendered.path), 0o600);
        assert_eq!(
            std::fs::read_to_string(&rendered.path).expect("read the config"),
            "{ mode: \"router\" }"
        );
    }

    /// A file that another user put at the old fixed path of the config, or
    /// a config of another daemon on the same port, is not the file that the
    /// daemon writes and runs.
    #[test]
    fn two_rendered_configs_of_one_port_are_two_files() {
        let parent = tempfile::tempdir().expect("temp dir");
        std::fs::write(parent.path().join("zenohd_config_7448.json5"), "planted")
            .expect("plant a file");

        let first = RenderedRouterConfig::write(parent.path(), 7448, "first").expect("write");
        let second = RenderedRouterConfig::write(parent.path(), 7448, "second").expect("write");

        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read_to_string(&first.path).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second.path).unwrap(), "second");
    }

    #[test]
    fn the_owner_removes_the_directory_of_a_rendered_config() {
        let parent = tempfile::tempdir().expect("temp dir");
        let rendered = RenderedRouterConfig::write(parent.path(), 7448, "{}").expect("write");
        let dir = rendered.path.parent().expect("directory").to_path_buf();

        drop(rendered);

        assert!(!dir.exists());
    }

    #[test]
    fn a_fork_leaves_the_directory_of_a_rendered_config_to_its_owner() {
        let parent = tempfile::tempdir().expect("temp dir");
        let mut rendered = RenderedRouterConfig::write(parent.path(), 7448, "{}").expect("write");
        let dir = rendered.path.parent().expect("directory").to_path_buf();
        // The value as a fork of its owner holds it.
        rendered.owner_pid = std::process::id().wrapping_add(1);

        drop(rendered);

        assert!(dir.join("zenohd_config_7448.json5").is_file());
    }
}
