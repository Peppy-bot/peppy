pub const NODE_CONFIG_FILE: &str = "peppy.json5";
pub const RUNTIME_CONFIG_VAR_NAME: &str = "PEPPY_RUNTIME_CONFIG";
/// The peppy output directory relative to node_dir (contains generated libraries).
pub const PEPPY_OUTPUT_DIR: &str = ".peppy";
/// The standard output directory for generated peppygen libraries relative to node_dir.
pub const PEPPYGEN_OUTPUT_PATH: &str = ".peppy/libs/peppygen";
pub const PEPPYLIB_OUTPUT_PATH: &str = ".peppy/libs/peppylib";
pub const DAEMON_STATE_FILE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

/// Filename of the CLI's cached OAuth credentials, stored under `~/.peppy/conf`
/// (i.e. `conf_dir().join(CREDENTIALS_FILE)`). Written `0600` by the `peppy
/// login` flow; never committed and never world-readable.
pub const CREDENTIALS_FILE: &str = "credentials.json5";

/// Overrides the peppy data root (the `.peppy` directory itself). Mirrors the
/// `PEPPY_HOME` install prefix from scripts/install.sh. When set and non-empty,
/// it is used verbatim as `PeppyDirs::root`.
pub const PEPPY_HOME_ENV: &str = "PEPPY_HOME";

pub const DEFAULT_MESSAGING_HOST: &str = "127.0.0.1";
pub const DEFAULT_MESSAGING_PORT: u16 = 7448;
pub const PEPPY_MESSAGING_PORT_VAR_NAME: &str = "PEPPY_MESSAGING_PORT";

pub const ALLOWED_CONFIG_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";

/// Reserved literal that occupies the `link_id` slot on the wire when a
/// producer is launched without an explicit binding. Consumers that opt into
/// the producer-default path subscribe with this literal as their `link_id`
/// filter. `pmi::DEFAULT_LINK_ID` re-exports this so the wire layer and the
/// config layer share one source of truth.
pub const DEFAULT_LINK_ID_SENTINEL: &str = "_";

/// Minimum Python version required by peppylib and peppygen projects (e.g. "3.11").
///
/// NOTE: When updating, also update the static files in `peppylib-py/`
/// (`Cargo.toml` abi3 feature, `pyproject.toml`, `pixi.toml`, `Readme.md`)
/// which cannot be programmatically derived from this constant.
pub const PYTHON_MIN_VERSION: &str = "3.11";

/// Maximum Python version supported (exclusive, e.g. "3.14").
/// Driven by pycapnp wheel availability (wheels not yet available for Python 3.14 as of Feb 2026).
pub const PYTHON_MAX_VERSION: &str = "3.14";

/// Default base container image for Rust nodes (Ubuntu 24.04 + Rust via rustup, build-essential).
pub const DEFAULT_RUST_BASE_IMAGE: &str = "tuatini/peppy-rust-cargo-base:latest";

/// Default base container image for Python nodes (Ubuntu 24.04 + Python 3, uv).
pub const DEFAULT_PYTHON_BASE_IMAGE: &str = "tuatini/peppy-python-uv-base:latest";

/// Default base container image for lightweight test containers (Google mirror — CI-friendly).
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
/// flip it). It is also **optional** — if never called, the environment
/// defaults to [`AppEnv::Dev`].
///
/// The only thing it influences is the *default* peppy data root: it shifts
/// [`peppy_root_dir`] (and therefore [`PeppyDirs::default`]) between
/// `~/.peppy` (prod) and `/tmp/.peppy` (dev). It has no effect on parsing,
/// validation, or any [`PeppyDirs`] constructed explicitly via
/// [`PeppyDirs::new`]. Code that wants a deterministic root — including tests —
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

    /// Build outputs from `node build` (`.sif` container images and `.tar.zst` archives).
    pub fn built_nodes_dir(&self) -> PathBuf {
        self.root.join("built_nodes")
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
    /// persistent build cache root used by `build_helpers::cache_dir` — never
    /// bulk-clean this directory.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Path to the stack operations log file.
    ///
    /// Records daemon-initiated lifecycle events (e.g. automatic instance
    /// removal after health-check failures) so users can audit what happened
    /// without digging through debug logs.
    pub fn stack_log_path(&self) -> PathBuf {
        self.root.join("stack_log.log")
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

    /// Cache directory for repo refresh results and other cached data.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Persistent Git checkouts shared across `node add` batches.
    /// Directories are keyed by `<slug>-<hash>` where the hash covers
    /// repo_url + ref so distinct refs coexist.
    pub fn git_checkouts_dir(&self) -> PathBuf {
        self.cache_dir().join("git_checkouts")
    }

    /// Persistent HTTP bundle extractions shared across `node add`
    /// batches. Directories are keyed by `<slug>-<hash>` over the URL
    /// + optional SHA256.
    pub fn http_bundles_dir(&self) -> PathBuf {
        self.cache_dir().join("http_bundles")
    }
}

/// Resolves the peppy data root (the `.peppy` directory).
///
/// Precedence:
/// 1. `PEPPY_HOME` if set and non-empty, used verbatim as the root.
/// 2. Otherwise the standard application data directory:
///    - Production: `~/.peppy`
///    - Development: `/tmp/.peppy`
///
/// `var_os` plus the empty-string guard means `PEPPY_HOME=` is treated as unset
/// rather than rooting at the empty path, matching `env_state_file_path()` in
/// `daemon_state.rs`.
pub fn peppy_root_dir() -> PathBuf {
    resolve_root(std::env::var_os(PEPPY_HOME_ENV))
}

/// Implementation of [`peppy_root_dir`] with the `PEPPY_HOME` value made
/// explicit, so the precedence can be tested without mutating process env.
fn resolve_root(home_override: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(home) = home_override.filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    match app_env() {
        AppEnv::Prod => dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".peppy"),
        AppEnv::Dev => std::env::temp_dir().join(".peppy"),
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

    #[test]
    fn resolve_root_uses_peppy_home_override_verbatim() {
        let root = resolve_root(Some("/custom/run-home".into()));
        assert_eq!(root, PathBuf::from("/custom/run-home"));
    }

    #[test]
    fn resolve_root_ignores_empty_override_and_falls_back_to_default() {
        // Empty PEPPY_HOME is treated as unset, not as the empty path.
        let with_empty = resolve_root(Some(std::ffi::OsString::new()));
        let unset = resolve_root(None);
        assert_eq!(with_empty, unset);
        // The fallback still ends in `.peppy` (Dev or Prod root).
        assert!(
            unset.ends_with(".peppy"),
            "fallback root: {}",
            unset.display()
        );
    }

    /// Ensures that static files in peppylib-py/ that cannot be programmatically
    /// templated stay in sync with the canonical PYTHON_MIN_VERSION/PYTHON_MAX_VERSION constants.
    #[test]
    fn python_version_consistency_in_static_files() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");

        let pyproject_path = peppylib_py_dir.join("pyproject.toml");
        let pyproject_contents = std::fs::read_to_string(&pyproject_path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", pyproject_path.display(), e));
        let min_spec_ok = pyproject_contents.contains(format!(">={}", PYTHON_MIN_VERSION).as_str())
            || pyproject_contents.contains(format!(">= {}", PYTHON_MIN_VERSION).as_str());
        let max_spec_ok = pyproject_contents.contains(format!("<{}", PYTHON_MAX_VERSION).as_str())
            || pyproject_contents.contains(format!("< {}", PYTHON_MAX_VERSION).as_str());
        assert!(
            pyproject_contents.contains("requires-python") && min_spec_ok && max_spec_ok,
            "File {} must declare requires-python with both min and max constraints: \
             expected >= {} and < {}",
            pyproject_path.display(),
            PYTHON_MIN_VERSION,
            PYTHON_MAX_VERSION,
        );

        let files_and_patterns: &[(&str, String)] = &[
            ("Readme.md", format!("Python >= {}", PYTHON_MIN_VERSION)),
            (
                "pixi.toml",
                format!(
                    "python = \">={},<{}\"",
                    PYTHON_MIN_VERSION, PYTHON_MAX_VERSION
                ),
            ),
            (
                "Cargo.toml",
                format!("abi3-py{}", PYTHON_MIN_VERSION.replace('.', "")),
            ),
        ];

        for (filename, expected_pattern) in files_and_patterns {
            let file_path = peppylib_py_dir.join(filename);
            let contents = std::fs::read_to_string(&file_path)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", file_path.display(), e));
            assert!(
                contents.contains(expected_pattern.as_str()),
                "File {} does not contain expected pattern '{}'. \
                 Update this file to match PYTHON_MIN_VERSION = \"{}\" / PYTHON_MAX_VERSION = \"{}\"",
                file_path.display(),
                expected_pattern,
                PYTHON_MIN_VERSION,
                PYTHON_MAX_VERSION,
            );
        }
    }
}
