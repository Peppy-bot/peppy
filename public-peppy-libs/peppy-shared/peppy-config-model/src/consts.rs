pub const NODE_CONFIG_FILE: &str = "peppy.json5";
pub const RUNTIME_CONFIG_VAR_NAME: &str = "PEPPY_RUNTIME_CONFIG";
/// The standard output directory for generated peppygen libraries relative to node_dir.
pub const PEPPYGEN_OUTPUT_PATH: &str = ".peppy/libs/peppygen";

/// Overrides the peppy data root (the `.peppy` directory itself). Mirrors the
/// `PEPPY_HOME` install prefix from scripts/install.sh. When set and non-empty,
/// the peppy `daemon-config` crate uses it verbatim as its data root
/// (`PeppyDirs::root`).
pub const PEPPY_HOME_ENV: &str = "PEPPY_HOME";

/// Overrides the daemon-global config document (peppy_config.json5).
/// When set and non-empty, the value is tried as a path to a config file
/// first; if no file can be read, it is parsed as an inline JSON5 document.
/// The source is loaded read-only: nothing is created, completed, or
/// rewritten, and a value that cannot be read, does not parse, fails
/// validation, or omits settings the running release defines is a startup
/// error. Empty or unset means the normal on-disk flow.
pub const PEPPY_CONFIG_ENV: &str = "PEPPY_CONFIG";

pub const DEFAULT_MESSAGING_HOST: &str = "127.0.0.1";
pub const DEFAULT_MESSAGING_PORT: u16 = 7448;

pub const ALLOWED_CONFIG_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-";

/// Reserved literal that occupies the `link_id` slot on the wire when a
/// producer is launched without an explicit binding. Consumers that opt into
/// the producer-default path subscribe with this literal as their `link_id`
/// filter. `pmi::DEFAULT_LINK_ID` re-exports this so the wire layer and the
/// config layer share one source of truth.
pub const DEFAULT_LINK_ID_SENTINEL: &str = "_";

/// Hyphen-to-underscore normalization applied to tag segments by the
/// generator (module paths) and the wire format (keyexpr segments). The
/// wire layer (`pmi::wire`) and the parse-time implements-collision check
/// both call this one definition, so the parse-time prediction of wire
/// behavior cannot drift from the wire itself.
pub fn normalize_tag(tag: &str) -> String {
    if tag.contains('-') {
        tag.replace('-', "_")
    } else {
        tag.to_string()
    }
}

/// Minimum Python version required by peppylib and peppygen projects (e.g. "3.11").
///
/// NOTE: When updating, also update the static files in `peppylib-py/`
/// (`Cargo.toml` abi3 feature, `pyproject.toml`, `pixi.toml`, `Readme.md`)
/// which cannot be programmatically derived from this constant.
pub const PYTHON_MIN_VERSION: &str = "3.11";

/// Maximum Python version supported (exclusive, e.g. "3.15").
/// Bounded by the newest CPython minor release pycapnp publishes wheels for.
pub const PYTHON_MAX_VERSION: &str = "3.15";

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures that static files in peppylib-py/ that cannot be programmatically
    /// templated stay in sync with the canonical PYTHON_MIN_VERSION/PYTHON_MAX_VERSION constants.
    #[test]
    fn python_version_consistency_in_static_files() {
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // peppylib-py was moved out of the peppy workspace into
        // public-peppy-libs/peppy-shared (resolves only in the superproject checkout).
        let peppylib_py_dir =
            manifest_dir.join("../../../public-peppy-libs/peppy-shared/peppylib-py");

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
