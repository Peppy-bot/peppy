/// The peppy output directory relative to node_dir (contains generated libraries).
pub const PEPPY_OUTPUT_DIR: &str = ".peppy";
pub const PEPPYLIB_OUTPUT_PATH: &str = ".peppy/libs/peppylib";
/// Filename of the CLI's cached OAuth credentials, stored under `~/.peppy/conf`
/// (i.e. `conf_dir().join(CREDENTIALS_FILE)`). Written `0600` by the `peppy
/// login` flow; never committed and never world-readable.
pub const CREDENTIALS_FILE: &str = "credentials.json5";
/// Filename of the platform selection, stored under `~/.peppy/conf` beside the
/// credentials: the workspace and the project the person selected as the
/// default target of the `peppy platform` commands.
pub const PLATFORM_SELECTION_FILE: &str = "platform_selection.json5";

/// Filename of the request headers of the log export, stored under
/// `~/.peppy/conf`: a JSON5 object of header name to value, such as an API
/// key. The daemon sets it to `0600` when it reads it.
const OTLP_HEADERS_FILE: &str = "otlp_headers.json5";

/// Filename of a repository's index, at the root of the tree peppy is
/// configured to scan. A repository states there what it publishes and where
/// each item is declared; without it the repository does not resolve.
pub const REPOSITORY_INDEX_FILE: &str = "peppy_repository.json5";

pub const PEPPY_MESSAGING_PORT_VAR_NAME: &str = "PEPPY_MESSAGING_PORT";

/// Environment variable that sets the size limit of the dev data root (see
/// [`RootLifetime::PerBoot`]), for example `20G`. Unset, the dev data root has
/// no size limit.
pub const PEPPY_DEV_ROOT_MAX_SIZE_ENV: &str = "PEPPY_DEV_ROOT_MAX_SIZE";

/// Filename of the daemon singleton lock under [`PeppyDirs::runtime_config_dir`].
const DAEMON_LOCK_FILE: &str = "daemon.lock";
/// Filename of the lock that serializes the clears of a per-boot data root,
/// under [`PeppyDirs::runtime_config_dir`].
const ROOT_CLEAR_LOCK_FILE: &str = "root_clear.lock";
/// Filename of the record of the boot that last used a per-boot data root,
/// under [`PeppyDirs::runtime_config_dir`].
const ROOT_BOOT_ID_FILE: &str = "boot_id";

/// Release tag the binary was built from, read at compile time from the
/// `PEPPY_GIT_TAG` environment variable the release build sets. A plain
/// `cargo build` has none.
pub const PEPPY_GIT_TAG: Option<&str> = option_env!("PEPPY_GIT_TAG");

/// Version the CLI and the daemon report for themselves: the release tag, or
/// `unknown` for a build without one.
pub const PEPPY_VERSION: &str = match PEPPY_GIT_TAG {
    Some(tag) => tag,
    None => "unknown",
};

/// The build this binary is, as far as a repository entry on
/// `@{peppy-release}` goes: a release build reads the hub tags of its own
/// version, any other build reads the hubs' `main`.
pub fn peppy_build() -> core_node_api::encoding::PeppyBuild {
    core_node_api::encoding::PeppyBuild::from_git_tag(PEPPY_GIT_TAG)
}

/// Default base container image for Rust nodes (Ubuntu 24.04 + Rust via rustup, build-essential).
pub const DEFAULT_RUST_BASE_IMAGE: &str = "peppybot/rust-cargo-base:latest";

/// Default base container image for Python nodes (Ubuntu 24.04 + Python 3, uv).
pub const DEFAULT_PYTHON_BASE_IMAGE: &str = "peppybot/python-uv-base:latest";

/// Default base container image for lightweight test containers (Google mirror, CI-friendly).
pub const DEFAULT_ALPINE_BASE_IMAGE: &str = "mirror.gcr.io/library/alpine:3.20";

// Application runtime environment (dev/prod) tracked internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Prod,
}

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static APP_ENV: OnceLock<AppEnv> = OnceLock::new();

/// Records the process-wide application environment (dev vs prod).
///
/// This is the crate's only mutable global state. It is **set-once**: the
/// first call wins and every later call is silently ignored (so a binary can
/// pin the environment at startup without callers downstream being able to
/// flip it). It is also **optional**: if never called, the environment
/// defaults to [`AppEnv::Dev`].
///
/// The only thing it influences is the *default* peppy data root: it shifts
/// [`peppy_root`] (and therefore [`PeppyDirs::default`]) between
/// `~/.peppy` (prod) and `~/.cache/peppy-dev` (dev). It has no effect on parsing,
/// validation, or any [`PeppyDirs`] constructed explicitly via
/// [`PeppyDirs::new`]. Code that wants a deterministic root, including tests,
/// should construct [`PeppyDirs::new`] directly rather than rely on this
/// global; that keeps it independent of call ordering across threads.
pub fn set_app_env(env: AppEnv) {
    APP_ENV.set(env).ok();
}

/// Returns the process-wide application environment, defaulting to
/// [`AppEnv::Dev`] if [`set_app_env`] was never called. Reads only; see
/// [`set_app_env`] for the set-once contract and what it affects.
pub fn app_env() -> AppEnv {
    *APP_ENV.get_or_init(|| AppEnv::Dev)
}

/// Directory layout for peppy data (added nodes, instances, logs, caches).
///
/// Threading this struct through production code instead of using a global static
/// ensures tests can run in parallel with fully isolated filesystem state.
#[derive(Clone, Debug)]
pub struct PeppyDirs {
    root: PathBuf,
}

impl PeppyDirs {
    /// Creates a `PeppyDirs` rooted at the given path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Build outputs from `node build`: one directory per node identity (see
    /// [`PeppyDirs::built_node_dir`]) holding the `.sif` container image or
    /// `.tar.zst` archive named after the fingerprint of the staged sources
    /// it was built from.
    pub fn built_nodes_dir(&self) -> PathBuf {
        self.root.join("built_nodes")
    }

    /// Directory holding the build artifact of one node identity,
    /// `<built_nodes_dir>/<node_name>_<node_tag>`. Callers validate the tag
    /// before splicing it into the path.
    pub fn built_node_dir(&self, node_name: &str, node_tag: &str) -> PathBuf {
        self.built_nodes_dir()
            .join(format!("{node_name}_{node_tag}"))
    }

    /// Extracted archives for running node instances.
    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    /// Log directory for `node add` operations.
    pub fn logs_dir_add(&self) -> PathBuf {
        self.root.join("logs").join("add")
    }

    /// Log directory for `node build` operations.
    pub fn logs_dir_build(&self) -> PathBuf {
        self.root.join("logs").join("build")
    }

    /// Log directory for `node run` operations.
    pub fn logs_dir_run(&self) -> PathBuf {
        self.root.join("logs").join("run")
    }

    /// Log directory for `stack launch` operations.
    pub fn logs_dir_launch(&self) -> PathBuf {
        self.root.join("logs").join("launch")
    }

    /// Runtime configuration directory.
    pub fn runtime_config_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    /// The daemon singleton lock file. Nothing ever unlinks it: two daemons
    /// that raced on an unlinked and recreated lock file would lock two
    /// different inodes behind the same path.
    pub fn daemon_lock_path(&self) -> PathBuf {
        self.runtime_config_dir().join(DAEMON_LOCK_FILE)
    }

    /// The lock that serializes the clears of a per-boot data root. Like
    /// [`Self::daemon_lock_path`], nothing ever unlinks it.
    pub fn root_clear_lock_path(&self) -> PathBuf {
        self.runtime_config_dir().join(ROOT_CLEAR_LOCK_FILE)
    }

    /// The record of the boot that last used a per-boot data root.
    pub fn root_boot_id_path(&self) -> PathBuf {
        self.runtime_config_dir().join(ROOT_BOOT_ID_FILE)
    }

    /// Directory holding the manifests and serve specs of built-in nodes,
    /// one subdirectory per node identity. A built-in node is registered in
    /// the stack from documents the daemon derives at add time rather than
    /// from a built artifact, and this is where those documents live.
    pub fn built_in_nodes_dir(&self) -> PathBuf {
        self.root.join("built_in")
    }

    /// Temporary download directory for HTTP-sourced node archives.
    pub fn http_downloads_dir(&self) -> PathBuf {
        self.root.join("http_downloads")
    }

    /// Temporary working directory for operations that may involve containers.
    ///
    /// On macOS with Lima, temp directories must be under `$HOME` to be
    /// visible inside the guest VM. Use this instead of `std::env::temp_dir()`.
    ///
    /// In production this resolves to `~/.peppy/tmp`, which doubles as the
    /// persistent build cache root used by `build_helpers::cache_dir`; never
    /// bulk-clean this directory. The per-boot dev root is the one exception:
    /// [`crate::per_boot_root`] clears it, this directory with it.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Directory holding peppy-managed runtime binaries.
    ///
    /// This resolves to `$PEPPY_HOME/bin`, defaulting to `~/.peppy/bin` in a
    /// production build. The installer places `peppy`, `zenohd`, and the
    /// container tools there by default, and peppy extracts its bundled Cap'n
    /// Proto compiler there on first use. A custom installer `PEPPY_BIN_DIR`
    /// relocates the installer-managed tools only; it does not change this
    /// runtime directory.
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Path to the stack log: each instance that ends as `finished` or
    /// `failed`, each change between `healthy` and `unhealthy`, and each
    /// report of records the log export discarded.
    pub fn stack_log_path(&self) -> PathBuf {
        self.root.join("stack_log.log")
    }

    /// Directory holding this machine's platform enrollment: the router peer
    /// record, its private key and certificate, and the project's trust anchor.
    /// Under [`Self::conf_dir`] beside the credentials file, since `peppy
    /// platform enroll` and `unenroll` own it the way `login` owns the session.
    pub fn peer_dir(&self) -> PathBuf {
        self.conf_dir().join("peer")
    }

    /// Shared Rust crate cache directory for a given cache key.
    pub fn rust_libs_cache_dir(&self, cache_key: &str) -> PathBuf {
        self.root.join("libs").join("rust").join(cache_key)
    }

    /// Shared Python library cache directory for a given cache key.
    pub fn python_libs_cache_dir(&self, cache_key: &str) -> PathBuf {
        self.root.join("libs").join("python").join(cache_key)
    }

    /// Configuration directory for user-editable config files (e.g. repositories.json5).
    pub fn conf_dir(&self) -> PathBuf {
        self.root.join("conf")
    }

    /// The request headers file of the log export.
    pub fn otlp_headers_path(&self) -> PathBuf {
        self.conf_dir().join(OTLP_HEADERS_FILE)
    }

    /// Cache directory for repo refresh results and other cached data.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Shared build cache bind mounted into Rust and Python container builds:
    /// the cargo registry and sccache artifacts of Rust builds, the uv
    /// packages and Python interpreters of Python builds, and the pinned
    /// downloads of both.
    pub fn container_build_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("container_build")
    }

    /// Persistent Git checkouts shared across `node add` batches.
    /// Directories are keyed by `<slug>-<hash>` where the hash covers
    /// repo_url + commit, so one directory holds exactly one tree.
    pub fn git_checkouts_dir(&self) -> PathBuf {
        self.cache_dir().join("git_checkouts")
    }
}

/// How long the content of a peppy data root lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootLifetime {
    /// Peppy never clears the root: a `PEPPY_HOME` root and the production
    /// root `~/.peppy`.
    Persistent,
    /// Peppy clears the root, except its configuration, at the first peppy
    /// process of each boot of the machine, and when the daemon starts with
    /// the root over the size limit of [`PEPPY_DEV_ROOT_MAX_SIZE_ENV`]: the
    /// default root of a dev build, `~/.cache/peppy-dev`. See
    /// [`crate::per_boot_root`].
    PerBoot,
}

/// A resolved peppy data root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeppyRoot {
    pub path: PathBuf,
    pub lifetime: RootLifetime,
}

/// Resolves the peppy data root.
///
/// Precedence:
/// 1. `PEPPY_HOME` if set and non-empty, used verbatim as the root.
/// 2. Otherwise the default root of the build:
///    - Production: `~/.peppy`
///    - Development: `~/.cache/peppy-dev`, a [`RootLifetime::PerBoot`] root
///
/// The dev root sits on disk under `$HOME`: a tmpfs `/tmp` is often limited
/// by a per-user quota that one container build fills, and on macOS the Lima
/// guest sees only paths under `$HOME`. It does not follow `XDG_CACHE_HOME`,
/// because the daemon a service manager starts does not get that variable
/// and must resolve the same root as the CLI.
///
/// `var_os` plus [`non_empty_env_path`] means `PEPPY_HOME=` is treated as
/// unset rather than rooting at the empty path.
pub fn peppy_root() -> PeppyRoot {
    resolve_root(
        std::env::var_os(config::consts::PEPPY_HOME_ENV),
        app_env(),
        dirs::home_dir(),
    )
}

/// The path of [`peppy_root`].
pub fn peppy_root_dir() -> PathBuf {
    peppy_root().path
}

/// Interprets an env var value as a path override. An empty value is
/// treated as unset rather than as the empty path, so `FOO=` behaves
/// like `unset FOO`. Takes the value (not the var name) so callers stay
/// testable without mutating process env.
pub fn non_empty_env_path(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Implementation of [`peppy_root`] with the `PEPPY_HOME` value, the build
/// and the home directory made explicit, so the precedence can be tested
/// without mutating process state.
fn resolve_root(
    home_override: Option<std::ffi::OsString>,
    env: AppEnv,
    user_home: Option<PathBuf>,
) -> PeppyRoot {
    if let Some(path) = non_empty_env_path(home_override) {
        return PeppyRoot {
            path,
            lifetime: RootLifetime::Persistent,
        };
    }
    let user_home = user_home.unwrap_or_else(std::env::temp_dir);
    match env {
        AppEnv::Prod => PeppyRoot {
            path: user_home.join(".peppy"),
            lifetime: RootLifetime::Persistent,
        },
        AppEnv::Dev => PeppyRoot {
            path: user_home.join(".cache").join("peppy-dev"),
            lifetime: RootLifetime::PerBoot,
        },
    }
}

/// Uses the standard application data directory (see [`peppy_root_dir`]).
impl Default for PeppyDirs {
    fn default() -> Self {
        Self::new(peppy_root_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/u"))
    }

    #[test]
    fn resolve_root_uses_peppy_home_override_verbatim_and_never_clears_it() {
        for env in [AppEnv::Dev, AppEnv::Prod] {
            let root = resolve_root(Some("/custom/run-home".into()), env, user_home());
            assert_eq!(
                root,
                PeppyRoot {
                    path: PathBuf::from("/custom/run-home"),
                    lifetime: RootLifetime::Persistent,
                }
            );
        }
    }

    #[test]
    fn resolve_root_ignores_empty_override_and_falls_back_to_default() {
        // Empty PEPPY_HOME is treated as unset, not as the empty path.
        for env in [AppEnv::Dev, AppEnv::Prod] {
            let with_empty = resolve_root(Some(std::ffi::OsString::new()), env, user_home());
            let unset = resolve_root(None, env, user_home());
            assert_eq!(with_empty, unset);
        }
    }

    #[test]
    fn resolve_root_roots_prod_at_a_persistent_home_peppy() {
        assert_eq!(
            resolve_root(None, AppEnv::Prod, user_home()),
            PeppyRoot {
                path: PathBuf::from("/home/u/.peppy"),
                lifetime: RootLifetime::Persistent,
            }
        );
    }

    #[test]
    fn resolve_root_roots_dev_at_a_per_boot_root_in_the_home_cache() {
        assert_eq!(
            resolve_root(None, AppEnv::Dev, user_home()),
            PeppyRoot {
                path: PathBuf::from("/home/u/.cache/peppy-dev"),
                lifetime: RootLifetime::PerBoot,
            }
        );
    }

    #[test]
    fn resolve_root_without_a_home_falls_back_to_the_temp_dir() {
        let root = resolve_root(None, AppEnv::Dev, None);
        assert_eq!(
            root.path,
            std::env::temp_dir().join(".cache").join("peppy-dev")
        );
    }

    #[test]
    fn non_empty_env_path_set_value_is_some() {
        assert_eq!(
            non_empty_env_path(Some("/some/path".into())),
            Some(PathBuf::from("/some/path"))
        );
    }

    #[test]
    fn non_empty_env_path_empty_value_is_none() {
        assert_eq!(non_empty_env_path(Some(std::ffi::OsString::new())), None);
    }

    #[test]
    fn non_empty_env_path_unset_is_none() {
        assert_eq!(non_empty_env_path(None), None);
    }
}
