use crate::error::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const NODE_CONFIG_FINGERPRINT_FILE: &str = "peppy.json5.sha256";

/// Generates the initial node fingerprint and copies the release fingerprint.
///
/// This function:
/// 1. Computes and writes the SHA256 hash of the node config to `{output_path}/peppy.json5.sha256`
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

    Ok(())
}

/// Creates the fingerprint files at the expected location for runtime checks.
///
/// This creates both:
/// 1. The config fingerprint (`peppy.json5.sha256`) in the peppygen output directory
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

        let config_contents = r#"{ peppy_schema: "node_v1", manifest: { name: "camera", tag: "v1" },
 execution: { language: "rust", run_cmd: ["./target/release/camera"] } }"#;
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

        let config_contents = r#"{ peppy_schema: "node_v1", manifest: { name: "camera", tag: "v1" },
 execution: { language: "rust", run_cmd: ["./target/release/camera"] } }"#;
        fs::write(&config_path, config_contents).expect("failed to write config");

        generate_node_config_fingerprint(&config_path, &generated_crate)
            .expect("failed to generate fingerprint");

        let written = fs::read_to_string(&fingerprint_path).unwrap();
        assert_eq!(
            written.trim(),
            fingerprint_for_bytes(config_contents.as_bytes())
        );
    }

    #[test]
    fn generate_node_config_fingerprint_returns_err_on_read_only_output_dir() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
        fs::write(&config_path, "test config content").expect("failed to write config");

        // Create output dir and make it read-only so the fingerprint file cannot be written
        let output_dir = tmp.path().join("readonly_output");
        fs::create_dir_all(&output_dir).expect("failed to create output dir");
        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o555))
            .expect("failed to set permissions");

        let result = generate_node_config_fingerprint(&config_path, &output_dir);
        assert!(
            result.is_err(),
            "should return Err when output directory is read-only"
        );

        // Restore write permissions for cleanup
        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o755))
            .expect("failed to restore permissions");
    }

    #[test]
    fn verify_codegen_fingerprint_round_trips_and_detects_tampering() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join(crate::consts::NODE_CONFIG_FILE);
        fs::write(&config_path, "original contents").expect("failed to write config");

        // `generate` writes to an absolute output dir, while `read`/`verify`
        // address that dir *relative to the config's parent*. Generating into
        // `<config_parent>/<rel_output>` makes the two sides line up.
        let rel_output = std::path::Path::new(crate::consts::PEPPYGEN_OUTPUT_PATH);
        generate_node_config_fingerprint(&config_path, tmp.path().join(rel_output))
            .expect("failed to generate fingerprint");

        // `read_codegen_fingerprint` returns exactly the digest that was stored.
        let read = read_codegen_fingerprint(&config_path, rel_output).expect("failed to read");
        assert_eq!(read, fingerprint_for_bytes(b"original contents"));

        // Verification passes while the config is unchanged.
        verify_codegen_fingerprint(&config_path, rel_output).expect("verify should pass");

        // Mutating the config makes the stored digest stale -> FingerprintMismatch.
        fs::write(&config_path, "tampered contents").expect("failed to rewrite config");
        let err = verify_codegen_fingerprint(&config_path, rel_output)
            .expect_err("verify should fail after tampering");
        assert!(
            matches!(err, crate::error::Error::FingerprintMismatch { .. }),
            "expected FingerprintMismatch, got {err:?}"
        );
    }

    #[test]
    fn read_codegen_fingerprint_errors_when_file_missing() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join(crate::consts::NODE_CONFIG_FILE);
        fs::write(&config_path, "contents").expect("failed to write config");

        // No fingerprint was ever generated, so the read must fail rather than
        // silently returning an empty/garbage digest.
        let rel_output = std::path::Path::new(crate::consts::PEPPYGEN_OUTPUT_PATH);
        assert!(read_codegen_fingerprint(&config_path, rel_output).is_err());
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
