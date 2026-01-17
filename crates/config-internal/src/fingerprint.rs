use crate::error::Result;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const NODE_CONFIG_FINGERPRINT_FILE: &str = "peppy.json5.sha256";

/// Generates the initial node fingerprint
pub fn generate_node_config_fingerprint(
    node_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let node_config = node_config.as_ref();
    let generated_crate = output_path.as_ref();
    let fingerprint_path = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);

    let config_bytes = fs::read(node_config)?;

    if let Some(dir) = fingerprint_path.parent() {
        fs::create_dir_all(dir)?;
    }

    let fingerprint = fingerprint_for_bytes(&config_bytes);
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))?;
    Ok(())
}

pub fn fingerprint_for_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Reads the codegen fingerprint from the generated output directory.
///
/// The fingerprint file is located at `{peppy_config_dir}/{output_path}/{fingerprint_file}`.
pub fn read_codegen_fingerprint(
    peppy_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<String> {
    let peppy_config_dir = peppy_config
        .as_ref()
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let fingerprint_path = peppy_config_dir
        .join(output_path.as_ref())
        .join(NODE_CONFIG_FINGERPRINT_FILE);

    fs::read_to_string(&fingerprint_path)
        .map(|s| s.trim().to_string())
        .map_err(Into::into)
}

/// Verifies that the node config fingerprint matches the one in the generated folder.
///
/// Compares the fingerprint stored in `{peppy_config_dir}/{output_path}/{fingerprint_file}`
/// against a freshly computed fingerprint of the config file.
pub fn verify_codegen_fingerprint(
    peppy_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let peppy_config = peppy_config.as_ref();
    let expected = read_codegen_fingerprint(peppy_config, output_path)?;
    let actual = fingerprint_for_bytes(&fs::read(peppy_config)?);

    if expected == actual {
        Ok(())
    } else {
        Err(crate::error::Error::FingerprintMismatch { expected, actual })
    }
}

/// Checks if a codegen fingerprint file exists at the expected location.
///
/// Returns the path to the fingerprint file if it exists, or None if it doesn't.
pub fn codegen_fingerprint_exists(
    peppy_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Option<std::path::PathBuf> {
    let peppy_config_dir = peppy_config
        .as_ref()
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let fingerprint_path = peppy_config_dir
        .join(output_path.as_ref())
        .join(NODE_CONFIG_FINGERPRINT_FILE);

    if fingerprint_path.exists() {
        Some(fingerprint_path)
    } else {
        None
    }
}

/// Creates the fingerprint file at the expected location for runtime checks.
#[cfg(feature = "test_helpers")]
pub fn create_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
    let fingerprint_dir = peppy_config_dir.join(output_path);
    fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");
    let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    let fingerprint = fingerprint_for_bytes(
        &fs::read(peppy_config_path).expect("peppy config should be readable"),
    );
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("fingerprint should be written");
}

/// Creates a fingerprint file with incorrect content to test mismatch errors.
#[cfg(feature = "test_helpers")]
pub fn create_wrong_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
    let fingerprint_dir = peppy_config_dir.join(output_path);
    fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");
    let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    fs::write(&fingerprint_path, "wrong_fingerprint_value\n")
        .expect("fingerprint should be written");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn generate_node_config_fingerprint_writes_expected_digest() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join(crate::consts::NODE_CONFIG_FILE);
        let generated_crate = prepare_generated_crate(&tmp);

        let config_contents =
            r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.1.0" } }"#;
        fs::write(&config_path, config_contents).expect("failed to write config");

        generate_node_config_fingerprint(&config_path, &generated_crate)
            .expect("failed to generate fingerprint");

        let written =
            fs::read_to_string(generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE)).unwrap();
        assert_eq!(
            written.trim(),
            fingerprint_for_bytes(config_contents.as_bytes())
        );
    }

    #[test]
    fn generate_node_config_fingerprint_overwrites_existing() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join(crate::consts::NODE_CONFIG_FILE);
        let generated_crate = prepare_generated_crate(&tmp);

        // Write initial fingerprint
        let fingerprint_path = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);
        fs::write(&fingerprint_path, "old_fingerprint\n").expect("failed to write old fingerprint");

        let config_contents =
            r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.1.0" } }"#;
        fs::write(&config_path, config_contents).expect("failed to write config");

        generate_node_config_fingerprint(&config_path, &generated_crate)
            .expect("failed to generate fingerprint");

        let written = fs::read_to_string(&fingerprint_path).unwrap();
        assert_eq!(
            written.trim(),
            fingerprint_for_bytes(config_contents.as_bytes())
        );
    }

    fn prepare_generated_crate(tmp: &TempDir) -> std::path::PathBuf {
        let crate_dir = tmp.path().join("generated_crate");
        fs::create_dir_all(crate_dir.join("src")).expect("failed to create src directory");

        fs::write(
            crate_dir.join("Cargo.toml"),
            r#"[package]
                name = "generated_crate"
                version = "0.1.0"
                edition = "2021"
            "#,
        )
        .expect("failed to write Cargo.toml");

        fs::write(crate_dir.join("src/lib.rs"), "pub fn generated() {}\n")
            .expect("failed to write lib.rs");

        crate_dir
    }
}
