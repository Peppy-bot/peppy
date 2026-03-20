pub const CORE_NODE_TOPIC_NAME: &str = "command";
pub const NODE_CONFIG_FILE: &str = "peppy.json5";
pub const RUNTIME_CONFIG_VAR_NAME: &str = "PEPPY_RUNTIME_CONFIG";
/// The peppy output directory relative to node_dir (contains generated libraries).
pub const PEPPY_OUTPUT_DIR: &str = ".peppy";
/// The standard output directory for generated peppygen libraries relative to node_dir.
pub const PEPPYGEN_OUTPUT_PATH: &str = ".peppy/libs/peppygen";
pub const PEPPYLIB_OUTPUT_PATH: &str = ".peppy/libs/peppylib";
pub const DAEMON_STATE_FILE_ENV: &str = "PEPPY_DAEMON_STATE_FILE";

pub const DEFAULT_MESSAGING_HOST: &str = "127.0.0.1";
pub const DEFAULT_MESSAGING_PORT: u16 = 7448;
pub const PEPPY_MESSAGING_PORT_VAR_NAME: &str = "PEPPY_MESSAGING_PORT";

pub const ALLOWED_CONFIG_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";

/// Minimum Python version required by peppylib and peppygen projects (e.g. "3.11").
///
/// NOTE: When updating, also update the static files in `peppylib-py/`
/// (`Cargo.toml` abi3 feature, `pyproject.toml`, `pixi.toml`, `Readme.md`)
/// which cannot be programmatically derived from this constant.
pub const PYTHON_MIN_VERSION: &str = "3.11";

/// Maximum Python version supported (exclusive, e.g. "3.14").
/// Driven by pycapnp wheel availability (wheels not yet available for Python 3.14 as of Feb 2026).
pub const PYTHON_MAX_VERSION: &str = "3.14";

/// Default base container image for Ubuntu-based nodes (ECR Public — no rate limits).
pub const DEFAULT_UBUNTU_BASE_IMAGE: &str = "public.ecr.aws/ubuntu/ubuntu:24.04";

/// Default base container image for lightweight test containers (ECR Public — no rate limits).
pub const DEFAULT_ALPINE_BASE_IMAGE: &str = "public.ecr.aws/docker/library/alpine:3.20";

// Application runtime environment (dev/prod) tracked internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Prod,
}

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static APP_ENV: OnceLock<AppEnv> = OnceLock::new();

/// Sets the application environment once. Subsequent calls are ignored.
pub fn set_app_env(env: AppEnv) {
    APP_ENV.set(env).ok();
}

/// Returns the current application environment, defaulting to Dev.
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

    /// Archived node snapshots from `node add`.
    pub fn added_nodes_dir(&self) -> PathBuf {
        self.root.join("added_nodes")
    }

    /// Extracted archives for running node instances.
    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    /// Log directory for `node add` operations.
    pub fn logs_dir_add(&self) -> PathBuf {
        self.root.join("logs").join("add")
    }

    /// Log directory for `node start` operations.
    pub fn logs_dir_start(&self) -> PathBuf {
        self.root.join("logs").join("start")
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
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Shared Rust crate cache directory for a given cache key.
    pub fn rust_libs_cache_dir(&self, cache_key: &str) -> PathBuf {
        self.root.join("libs").join("rust").join(cache_key)
    }

    /// Shared Python library cache directory for a given cache key.
    pub fn python_libs_cache_dir(&self, cache_key: &str) -> PathBuf {
        self.root.join("libs").join("python").join(cache_key)
    }
}

/// Uses the standard application data directory.
///
/// - Production: `~/.peppy`
/// - Development: `/tmp/.peppy`
impl Default for PeppyDirs {
    fn default() -> Self {
        let root = match app_env() {
            AppEnv::Prod => dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".peppy"),
            AppEnv::Dev => std::env::temp_dir().join(".peppy"),
        };
        Self { root }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
