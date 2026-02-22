pub const DAEMON_NODE_TOPIC_NAME: &str = "command";
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

// Application runtime environment (dev/prod) tracked internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Prod,
}

use std::sync::OnceLock;

static APP_ENV: OnceLock<AppEnv> = OnceLock::new();
static PEPPY_DATA_DIR_OVERRIDE: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Overrides the peppy data directory for the current process.
///
/// This is primarily intended for tests to isolate filesystem state.
pub fn set_peppy_data_dir_override(path: std::path::PathBuf) {
    PEPPY_DATA_DIR_OVERRIDE.set(path).ok();
}

/// Sets the application environment once. Subsequent calls are ignored.
pub fn set_app_env(env: AppEnv) {
    APP_ENV.set(env).ok();
}

/// Returns the current application environment, defaulting to Dev.
pub fn app_env() -> AppEnv {
    *APP_ENV.get_or_init(|| AppEnv::Dev)
}

/// Returns the base peppy data directory.
/// Can be overridden with PEPPY_DATA_DIR environment variable.
/// In production: ~/.peppy
/// In development: /tmp/.peppy
pub fn peppy_data_dir() -> std::path::PathBuf {
    if let Some(path) = PEPPY_DATA_DIR_OVERRIDE.get() {
        return path.clone();
    }

    // Check for environment variable override first
    if let Some(override_path) = std::env::var_os("PEPPY_DATA_DIR") {
        return std::path::PathBuf::from(override_path);
    }

    match app_env() {
        AppEnv::Prod => dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".peppy"),
        AppEnv::Dev => std::env::temp_dir().join(".peppy"),
    }
}

/// Returns the nodes cache directory.
/// In production: ~/.peppy/nodes
/// In development: /tmp/.peppy/nodes
pub fn nodes_cache_dir() -> std::path::PathBuf {
    peppy_data_dir().join("nodes")
}

/// Returns the node instances directory (extracted archives for running nodes).
/// In production: ~/.peppy/instances
/// In development: /tmp/.peppy/instances
pub fn instances_dir() -> std::path::PathBuf {
    peppy_data_dir().join("instances")
}

/// Returns the add logs cache directory.
/// In production: ~/.peppy/logs/add
/// In development: /tmp/.peppy/logs/add
pub fn logs_dir_add() -> std::path::PathBuf {
    peppy_data_dir().join("logs").join("add")
}

/// Returns the start logs cache directory.
/// In production: ~/.peppy/logs/start
/// In development: /tmp/.peppy/logs/start
pub fn logs_dir_start() -> std::path::PathBuf {
    peppy_data_dir().join("logs").join("start")
}

/// Returns the launch logs cache directory.
/// In production: ~/.peppy/logs/launch
/// In development: /tmp/.peppy/logs/launch
pub fn logs_dir_launch() -> std::path::PathBuf {
    peppy_data_dir().join("logs").join("launch")
}

/// Returns the runtime config directory path.
pub fn runtime_config_dir() -> std::path::PathBuf {
    peppy_data_dir().join("runtime")
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
