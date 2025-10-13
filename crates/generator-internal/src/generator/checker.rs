use crate::error::Result;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const NODE_CONFIG_FINGERPRINT_FILE: &str = ".peppygen/node_config.sha256";

/// Generates the initial node fingerprint
pub fn generate_node_config_fingerprint(
    node_config: impl AsRef<Path>,
    generated_crate: impl AsRef<Path>,
) -> Result<()> {
    let node_config = node_config.as_ref();
    let generated_crate = generated_crate.as_ref();
    let fingerprint_path = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);

    let config_bytes = fs::read(node_config)?;

    if let Some(dir) = fingerprint_path.parent() {
        fs::create_dir_all(dir)?;
    }

    let fingerprint = fingerprint_for_bytes(&config_bytes);
    fs::write(&fingerprint_path, format!("{fingerprint}\n"))?;
    Ok(())
}

/// Checks if the generated functions signature maps to the node config passed in parameters
/// This function is called from within the peppygen lib and the check is made at runtime, not by the `generator-internal` crate
pub fn check_node_config_up_to_date(
    node_config: impl AsRef<Path>,
    generated_crate: impl AsRef<Path>,
) {
    let node_config = node_config.as_ref();
    let generated_crate = generated_crate.as_ref();
    let fingerprint_path = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);

    let config_bytes = fs::read(node_config).unwrap_or_else(|err| {
        panic!(
            "Failed to read node config `{}`: {err}",
            node_config.display()
        )
    });

    let expected_fingerprint = fs::read_to_string(&fingerprint_path).unwrap_or_else(|err| {
        panic!(
            "Generated crate `{}` is missing fingerprint file `{}`: {err}. \
             Regenerate the bindings to keep them in sync with `{}`.",
            generated_crate.display(),
            fingerprint_path.display(),
            node_config.display()
        )
    });

    let expected_fingerprint = expected_fingerprint.trim();
    let actual_fingerprint = fingerprint_for_bytes(&config_bytes);

    if expected_fingerprint != actual_fingerprint {
        // TODO: Update the message with the actual `peppy` command to run
        panic!(
            "The peppy.config file is out of sync with its generated code. \
             Regenerate the bindings to ensure signatures stay in sync."
        );
    }
}

fn fingerprint_for_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, panic::AssertUnwindSafe};
    use tempfile::TempDir;

    #[test]
    fn passes_when_fingerprints_match() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
        let generated_crate = prepare_generated_crate(&tmp);
        let fingerprint_file = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);

        let config_contents =
            r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.1.0" } }"#;
        fs::write(&config_path, config_contents).expect("failed to write config");

        let fingerprint = fingerprint_for_bytes(config_contents.as_bytes());
        fs::write(&fingerprint_file, format!("{fingerprint}\n"))
            .expect("failed to write fingerprint file");

        check_node_config_up_to_date(&config_path, &generated_crate);
    }

    #[test]
    fn panics_when_fingerprint_is_missing() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
        let generated_crate = prepare_generated_crate(&tmp);
        fs::write(&config_path, "{}").expect("failed to write config");

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            check_node_config_up_to_date(&config_path, &generated_crate);
        }));

        let panic_message = extract_panic_message(result.expect_err("expected panic"));
        assert!(
            panic_message.contains("missing fingerprint file"),
            "unexpected panic message: {panic_message}"
        );
        assert!(
            panic_message.contains(NODE_CONFIG_FINGERPRINT_FILE),
            "panic should mention fingerprint file path: {panic_message}"
        );
    }

    #[test]
    fn panics_when_fingerprint_differs() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
        let generated_crate = prepare_generated_crate(&tmp);
        let fingerprint_file = generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE);

        let original_config =
            r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.1.0" } }"#;
        fs::write(&config_path, original_config).expect("failed to write config");

        let fingerprint = fingerprint_for_bytes(original_config.as_bytes());
        fs::write(&fingerprint_file, format!("{fingerprint}\n"))
            .expect("failed to write fingerprint file");

        let updated_config = r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.2.0" } }"#;
        fs::write(&config_path, updated_config).expect("failed to overwrite config");

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            check_node_config_up_to_date(&config_path, &generated_crate);
        }));

        let panic_message = extract_panic_message(result.expect_err("expected panic"));
        assert!(
            panic_message.contains("peppy.config file is out of sync"),
            "unexpected panic message: {panic_message}"
        );
        assert!(
            panic_message.contains("Regenerate the bindings"),
            "panic should mention regeneration hint: {panic_message}"
        );
    }

    #[test]
    fn generate_node_config_fingerprint_writes_expected_digest() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
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
    fn generate_node_config_fingerprint_creates_directory() {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let config_path = tmp.path().join("peppy.json5");
        let generated_crate = prepare_generated_crate(&tmp);
        let fingerprint_dir = generated_crate.join(".peppygen");
        fs::remove_dir_all(&fingerprint_dir).expect("failed to remove .peppygen");

        let config_contents =
            r#"{ schema_version: 1, manifest: { name: "camera", tag: "0.1.0" } }"#;
        fs::write(&config_path, config_contents).expect("failed to write config");

        generate_node_config_fingerprint(&config_path, &generated_crate)
            .expect("failed to generate fingerprint");

        assert!(
            fingerprint_dir.exists(),
            "expected fingerprint directory to be recreated"
        );
        let written =
            fs::read_to_string(generated_crate.join(NODE_CONFIG_FINGERPRINT_FILE)).unwrap();
        assert_eq!(
            written.trim(),
            fingerprint_for_bytes(config_contents.as_bytes())
        );
    }

    fn extract_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        match payload.downcast::<String>() {
            Ok(msg) => *msg,
            Err(payload) => match payload.downcast::<&'static str>() {
                Ok(msg) => msg.to_string(),
                Err(_) => "<non-string panic payload>".to_string(),
            },
        }
    }

    fn prepare_generated_crate(tmp: &TempDir) -> std::path::PathBuf {
        let crate_dir = tmp.path().join("generated_crate");
        fs::create_dir_all(crate_dir.join("src")).expect("failed to create src directory");
        fs::create_dir_all(crate_dir.join(".peppygen")).expect("failed to create .peppygen");

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
