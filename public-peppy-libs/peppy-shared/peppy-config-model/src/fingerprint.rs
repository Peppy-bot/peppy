use crate::error::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const NODE_CONFIG_FINGERPRINT_FILE: &str = "peppy.json5.sha256";

/// Resolves the canonical fingerprint file path shared by generate/read/verify.
///
/// `output_path` is interpreted relative to the directory containing
/// `node_config` (e.g. `PEPPYGEN_OUTPUT_PATH`), yielding
/// `{node_config_parent}/{output_path}/peppy.json5.sha256`. Keeping every
/// caller on this one helper guarantees the writer and the readers agree on the
/// exact location.
fn node_config_fingerprint_path(node_config: &Path, output_path: &Path) -> std::path::PathBuf {
    let config_dir = node_config.parent().unwrap_or_else(|| Path::new("."));
    config_dir
        .join(output_path)
        .join(NODE_CONFIG_FINGERPRINT_FILE)
}

/// Generates the node config fingerprint next to the generated peppygen output.
///
/// Computes the SHA256 hash of `node_config` and writes it to
/// `{node_config_parent}/{output_path}/peppy.json5.sha256` — the same location
/// [`read_codegen_fingerprint`] and [`verify_codegen_fingerprint`] read from.
pub fn generate_node_config_fingerprint(
    node_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let node_config = node_config.as_ref();
    let fingerprint_path = node_config_fingerprint_path(node_config, output_path.as_ref());

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

crate::hex_identity!(
    /// The SHA-256 of the bytes of the file that declares one item.
    ///
    /// What a machine compares to decide whether it already holds the same
    /// item another machine is talking about. Provenance answers where
    /// something came from; this answers whether it is the same thing, which
    /// is the question that authorises reuse.
    ManifestFingerprint,
    ManifestFingerprintError {
        Empty = "fingerprint is empty",
        NotASha256 = "fingerprint is not 64 hexadecimal characters: {0}",
    },
    64
);

impl ManifestFingerprint {
    /// The fingerprint of `bytes`, which is how every fingerprint peppy
    /// records is produced.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(fingerprint_for_bytes(bytes))
    }
}

/// Reads the codegen fingerprint from the generated output directory.
///
/// The fingerprint file is located at `{peppy_config_dir}/{output_path}/{fingerprint_file}`.
pub fn read_codegen_fingerprint(
    peppy_config: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
) -> Result<String> {
    let fingerprint_path =
        node_config_fingerprint_path(peppy_config.as_ref(), output_path.as_ref());

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
#[cfg(feature = "fingerprint_test_helpers")]
pub fn create_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let fingerprint_path = node_config_fingerprint_path(peppy_config_path, output_path);
    if let Some(dir) = fingerprint_path.parent() {
        fs::create_dir_all(dir).expect("fingerprint dir should be created");
    }

    // Create config fingerprint in peppygen output directory
    let fingerprint = fingerprint_for_bytes(
        &fs::read(peppy_config_path).expect("peppy config should be readable"),
    );
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))
        .expect("fingerprint should be written");
}

/// Creates a config fingerprint file with incorrect content to test mismatch errors.
#[cfg(feature = "fingerprint_test_helpers")]
pub fn create_wrong_codegen_fingerprint(peppy_config_path: &Path, output_path: &Path) {
    let fingerprint_path = node_config_fingerprint_path(peppy_config_path, output_path);
    if let Some(dir) = fingerprint_path.parent() {
        fs::create_dir_all(dir).expect("fingerprint dir should be created");
    }
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

        let config_contents = r#"{ peppy_schema: "node/v1", manifest: { name: "camera", tag: "v1" },
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

        let config_contents = r#"{ peppy_schema: "node/v1", manifest: { name: "camera", tag: "v1" },
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

        // `generate`, `read`, and `verify` all resolve the fingerprint relative
        // to the config's parent, so the same `output_path` feeds every side.
        let rel_output = std::path::Path::new(crate::consts::PEPPYGEN_OUTPUT_PATH);
        generate_node_config_fingerprint(&config_path, rel_output)
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
    #[test]
    fn a_manifest_fingerprint_is_the_lowercase_sha256_of_the_bytes() {
        let fingerprint = ManifestFingerprint::of_bytes(b"contents");
        assert_eq!(fingerprint.as_str(), fingerprint_for_bytes(b"contents"));
        assert_eq!(fingerprint.as_str().len(), 64);

        let upper = fingerprint.as_str().to_ascii_uppercase();
        let parsed = ManifestFingerprint::parse(&format!("  {upper} ")).expect("parses");
        assert_eq!(parsed, fingerprint, "parsing trims and lowercases");
        assert!(parsed == fingerprint.as_str());
        assert_eq!(parsed.to_string(), fingerprint.as_str());
    }

    #[test]
    fn a_manifest_fingerprint_must_be_64_hex_characters() {
        assert_eq!(
            ManifestFingerprint::parse("  "),
            Err(ManifestFingerprintError::Empty)
        );
        assert_eq!(
            ManifestFingerprint::parse("abc"),
            Err(ManifestFingerprintError::NotASha256("abc".to_string()))
        );
        let not_hex = "g".repeat(64);
        assert_eq!(
            ManifestFingerprint::parse(&not_hex),
            Err(ManifestFingerprintError::NotASha256(not_hex))
        );
        let error = serde_json::from_str::<ManifestFingerprint>("\"abc\"")
            .expect_err("deserialization routes through parse")
            .to_string();
        assert!(error.contains("not 64 hexadecimal characters"), "{error}");
    }
}
