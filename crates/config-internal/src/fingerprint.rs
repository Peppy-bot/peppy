use crate::error::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const NODE_CONFIG_FINGERPRINT_FILE: &str = "peppy.json5.sha256";
// This extra fingerprint tracks changes to the peppy client across releases
const RELEASE_FINGERPRINT_FILE: &str = "git.hash";

/// Generates the initial node fingerprint and copies the release fingerprint.
///
/// This function:
/// 1. Computes and writes the SHA256 hash of the node config to `{output_path}/peppy.json5.sha256`
/// 2. Copies the release fingerprint (`git.hash`) from the peppy data directory
///    to the node's `.peppy/git.hash`
///
/// Both fingerprints are required and must be created successfully.
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

    // Copy release fingerprint from peppy data directory to node's .peppy directory
    // output_path is .peppy/libs/peppygen, so .peppy is two levels up
    let data_dir = crate::consts::peppy_data_dir();
    let source_release_fingerprint = data_dir.join(RELEASE_FINGERPRINT_FILE);
    if !source_release_fingerprint.exists() {
        return Err(crate::error::Error::ReleaseFingerprintMissing(format!(
            "release fingerprint not found at {}",
            source_release_fingerprint.display()
        )));
    }

    let node_peppy_dir = generated_crate
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(generated_crate);
    let dest_release_fingerprint = node_peppy_dir.join(RELEASE_FINGERPRINT_FILE);
    fs::copy(&source_release_fingerprint, &dest_release_fingerprint)?;

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

/// Verifies that both the node config fingerprint and release fingerprint match.
///
/// This function verifies:
/// 1. The config fingerprint stored in `{peppy_config_dir}/{output_path}/peppy.json5.sha256`
///    matches a freshly computed fingerprint of the config file.
/// 2. The release fingerprint stored in `{peppy_config_dir}/.peppy/git.hash`
///    matches the one in the peppy data directory.
///
/// Both fingerprints must exist for verification to pass.
pub fn verify_codegen_fingerprint(
    peppy_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let peppy_config = peppy_config.as_ref();
    let output_path = output_path.as_ref();

    // Verify config fingerprint
    let expected = read_codegen_fingerprint(peppy_config, output_path)?;
    let actual = fingerprint_for_bytes(&fs::read(peppy_config)?);

    if expected != actual {
        return Err(crate::error::Error::FingerprintMismatch { expected, actual });
    }

    // Verify release fingerprint (both must exist)
    // Release fingerprint is stored in node_dir/.peppy/git.hash
    let peppy_config_dir = peppy_config.parent().unwrap_or_else(|| Path::new("."));
    let node_release_fingerprint_path = peppy_config_dir
        .join(".peppy")
        .join(RELEASE_FINGERPRINT_FILE);

    let data_dir = crate::consts::peppy_data_dir();
    let current_release_fingerprint_path = data_dir.join(RELEASE_FINGERPRINT_FILE);

    if !node_release_fingerprint_path.exists() {
        return Err(crate::error::Error::ReleaseFingerprintMissing(format!(
            "node release fingerprint not found at {}",
            node_release_fingerprint_path.display()
        )));
    }

    if !current_release_fingerprint_path.exists() {
        return Err(crate::error::Error::ReleaseFingerprintMissing(format!(
            "release fingerprint not found at {}",
            current_release_fingerprint_path.display()
        )));
    }

    let node_version = fs::read_to_string(&node_release_fingerprint_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let current_version = fs::read_to_string(&current_release_fingerprint_path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if node_version != current_version {
        return Err(crate::error::Error::ReleaseFingerprintMismatch {
            node_version,
            current_version,
        });
    }

    Ok(())
}

/// Creates the fingerprint files at the expected location for runtime checks.
///
/// This creates both:
/// 1. The config fingerprint (`peppy.json5.sha256`) in the peppygen output directory
/// 2. A matching release fingerprint (`git.hash`) in both the peppy data directory
///    and the node's `.peppy` directory
#[cfg(feature = "test_helpers")]
pub fn create_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
    let fingerprint_dir = peppy_config_dir.join(output_path);
    fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");

    // Create config fingerprint in peppygen output directory
    let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    let fingerprint = fingerprint_for_bytes(
        &fs::read(peppy_config_path).expect("peppy config should be readable"),
    );
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("fingerprint should be written");

    // Create matching release fingerprints in both locations
    // Always write a consistent value to ensure test isolation
    let data_dir = crate::consts::peppy_data_dir();
    fs::create_dir_all(&data_dir).expect("peppy data dir should be created");
    let data_release_fingerprint = data_dir.join(RELEASE_FINGERPRINT_FILE);

    // Node's release fingerprint is in node_dir/.peppy/git.hash
    let node_peppy_dir = peppy_config_dir.join(".peppy");
    fs::create_dir_all(&node_peppy_dir).expect("node peppy dir should be created");
    let node_release_fingerprint = node_peppy_dir.join(RELEASE_FINGERPRINT_FILE);

    let release_version = "test_release_version\n";
    fs::write(&data_release_fingerprint, release_version)
        .expect("should be able to write data release fingerprint");
    fs::write(&node_release_fingerprint, release_version)
        .expect("should be able to write node release fingerprint");
}

/// Creates a config fingerprint file with incorrect content to test mismatch errors.
#[cfg(feature = "test_helpers")]
pub fn create_wrong_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
    let fingerprint_dir = peppy_config_dir.join(output_path);
    fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");
    let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    fs::write(&fingerprint_path, "wrong_fingerprint_value\n")
        .expect("fingerprint should be written");
}

/// Creates a release fingerprint file with incorrect content to test release mismatch errors.
///
/// This function:
/// 1. Creates a valid config fingerprint
/// 2. Creates a "current" release fingerprint in the peppy data directory
/// 3. Creates a mismatched release fingerprint in the node's `.peppy` directory
///
/// This simulates the scenario where a node was generated with a different peppy version.
#[cfg(feature = "test_helpers")]
pub fn create_wrong_release_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    // First create a valid config fingerprint (without release fingerprint copy)
    let peppy_config_dir = peppy_config_path.parent().unwrap_or(Path::new("."));
    let fingerprint_dir = peppy_config_dir.join(output_path);
    fs::create_dir_all(&fingerprint_dir).expect("fingerprint dir should be created");

    // Create config fingerprint
    let fingerprint_path = fingerprint_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    let fingerprint = fingerprint_for_bytes(
        &fs::read(peppy_config_path).expect("peppy config should be readable"),
    );
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("fingerprint should be written");

    // Create a "current" release fingerprint in the peppy data directory
    let data_dir = crate::consts::peppy_data_dir();
    fs::create_dir_all(&data_dir).expect("peppy data dir should be created");
    let current_release_fingerprint = data_dir.join(RELEASE_FINGERPRINT_FILE);
    fs::write(&current_release_fingerprint, "current_release_version\n")
        .expect("current release fingerprint should be written");

    // Create a mismatched release fingerprint in the node's .peppy directory
    let node_peppy_dir = peppy_config_dir.join(".peppy");
    fs::create_dir_all(&node_peppy_dir).expect("node peppy dir should be created");
    let release_fingerprint_path = node_peppy_dir.join(RELEASE_FINGERPRINT_FILE);
    fs::write(
        &release_fingerprint_path,
        "wrong_release_fingerprint_value\n",
    )
    .expect("release fingerprint should be written");
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
