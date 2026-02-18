mod common;
pub(crate) mod naming;
#[cfg(test)]
#[macro_use]
mod test_helpers;
pub mod python;
pub mod rust;
pub mod types;

use crate::error::{Error, Result};
use config::{
    consts::NODE_CONFIG_FILE,
    node::{NodeConfigParser, PeppygenLanguage},
};
use python::PythonGenerator;
use rust::RustGenerator;
use std::{fs, path::Path};
use types::{DeploymentInterface, InterfaceVariant, LanguageGenerator};

/// Generate an interface library for the given language from a node directory.
///
/// This function reads the `peppy.json5` configuration file from the `node_dir`,
/// extracts the exposed interfaces, combines them with the provided subscribed interfaces,
/// and generates a library for the specified programming language.
/// The library is generated at `node_dir/.peppy/libs/peppygen`.
///
/// # Arguments
/// * `language` - The language to generate for (Rust or Python)
/// * `node_dir` - Path to the node directory containing `peppy.json5`
/// * `subscribed_interfaces` - Subscribed interfaces with resolved message formats from dependency nodes
///
/// # Errors
/// Returns an error if:
/// - The `peppy.json5` file is not found in `node_dir`
/// - The configuration file cannot be parsed
/// - Code generation fails
pub fn generate_peppygen_lib(
    language: PeppygenLanguage,
    node_dir: impl AsRef<Path>,
    subscribed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
) -> Result<()> {
    let node_dir = node_dir.as_ref();
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);

    let peppy_dir = node_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir)?;
    std::fs::write(peppy_dir.join("git.hash"), git_hash)?;

    if !node_config_path.exists() {
        return Err(Error::NodeNotFound(node_dir.display().to_string()));
    }

    let node_config = NodeConfigParser::from_path(&node_config_path)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

    let mut interfaces = collect_exposed_interfaces(&node_config);
    // Add the subscribed interfaces with resolved message formats
    interfaces.extend(subscribed_interfaces);

    // Create the output directory
    let output_dir = node_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    fs::create_dir_all(&output_dir)?;

    let result = match language {
        PeppygenLanguage::Rust => {
            let mut rust_generator = RustGenerator::new();
            rust_generator.set_parameters(node_config.parameters);
            generate_with_backend(rust_generator, &interfaces, &output_dir, node_dir)?;
            // Create or update the node's Cargo.toml with peppygen dependency
            ensure_node_cargo_toml(node_dir, node_config.manifest.name.as_str())?;
            // Patch any user-declared deps that overlap with precompiled crates
            sync_cargo_patches(node_dir)?;
            Ok(())
        }
        PeppygenLanguage::Python => {
            let mut python_generator = PythonGenerator::new();
            python_generator.set_parameters(node_config.parameters);
            generate_with_backend(python_generator, &interfaces, &output_dir, node_dir)
        }
    };

    // Lastly generate the codegen fingerprint based on the peppy.json5 config file
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    config::fingerprint::generate_node_config_fingerprint(&node_config_path, &output_dir)?;

    result
}

/// Collects all exposed interfaces from a NodeConfig into DeploymentInterface instances.
fn collect_exposed_interfaces(config: &config::node::NodeConfig) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::new();

    if let Some(exposes) = &config.interfaces.exposes {
        if let Some(topics) = &exposes.topics {
            for topic in topics {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedTopic(
                    topic.clone(),
                )));
            }
        }

        if let Some(services) = &exposes.services {
            for service in services {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedService(
                    service.clone(),
                )));
            }
        }

        if let Some(actions) = &exposes.actions {
            for action in actions {
                interfaces.push(DeploymentInterface::new(InterfaceVariant::ExposedAction(
                    action.clone(),
                )));
            }
        }
    }

    interfaces
}

fn generate_with_backend<B>(
    mut backend: B,
    interfaces: &[DeploymentInterface],
    output_dir: &Path,
    node_dir: &Path,
) -> Result<()>
where
    B: LanguageGenerator,
{
    for interface in interfaces {
        interface.register_with(&mut backend)?;
    }
    backend.build(output_dir, node_dir)
}

/// Synchronizes `[patch.crates-io]` entries in the node's Cargo.toml for any
/// user-declared dependencies that match precompiled crates.
///
/// When a user adds a registry crate (e.g. `tokio = "1.47"`) that is part of the
/// precompiled runtime, Cargo would normally download and compile it from scratch,
/// producing types incompatible with the precompiled version. This function
/// redirects those dependencies to thin stub crates that re-export from peppygen,
/// preventing the "multiple versions of crate" type mismatch.
///
/// Only crates explicitly listed in `[dependencies]` are patched — the section
/// stays proportional to what the user declared.
fn sync_cargo_patches(node_dir: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

    let cargo_toml_path = node_dir.join("Cargo.toml");
    if !cargo_toml_path.exists() {
        return Ok(());
    }

    let stubbable = rust::precompiled::stubbable_crates();
    if stubbable.is_empty() {
        return Ok(());
    }

    let contents = fs::read_to_string(&cargo_toml_path)?;
    let mut doc: DocumentMut = contents.parse().map_err(|e| {
        Error::Io(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("failed to parse Cargo.toml: {}", e),
        ))
    })?;

    // Build lookup from package_name → PrecompiledCrate
    let stubbable_map: std::collections::HashMap<&str, &rust::precompiled::PrecompiledCrate> =
        stubbable
            .iter()
            .map(|c| (c.package_name.as_str(), c))
            .collect();

    // Find user dependencies that match precompiled crates
    let mut patches_needed: Vec<&rust::precompiled::PrecompiledCrate> = Vec::new();
    if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
        for (dep_name, _) in deps.iter() {
            if dep_name == "peppygen" {
                continue;
            }
            if let Some(krate) = stubbable_map.get(dep_name) {
                patches_needed.push(krate);
            }
        }
    }

    // Remove existing [patch] section to regenerate cleanly
    doc.remove("patch");

    if !patches_needed.is_empty() {
        patches_needed.sort_by(|a, b| a.package_name.cmp(&b.package_name));

        let mut patch_table = Table::new();
        patch_table.set_implicit(true);
        let mut crates_io = Table::new();

        for krate in &patches_needed {
            let mut entry = InlineTable::new();
            entry.insert(
                "path",
                format!(
                    "{}/patches/{}",
                    config::consts::PEPPY_OUTPUT_DIR,
                    krate.package_name
                )
                .into(),
            );
            crates_io.insert(&krate.package_name, value(entry));
        }

        patch_table.insert("crates-io", Item::Table(crates_io));
        doc.insert("patch", Item::Table(patch_table));
    }

    fs::write(&cargo_toml_path, doc.to_string())?;
    Ok(())
}

/// Creates or updates the node's Cargo.toml with the peppygen dependency.
///
/// If the Cargo.toml doesn't exist, it creates a new one with the node name.
/// If it exists, it ensures the peppygen dependency is present while preserving formatting.
fn ensure_node_cargo_toml(node_dir: &Path, node_name: &str) -> Result<()> {
    use std::io::ErrorKind;
    use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

    let cargo_toml_path = node_dir.join("Cargo.toml");

    let mut doc: DocumentMut = if cargo_toml_path.exists() {
        let contents = fs::read_to_string(&cargo_toml_path)?;
        contents.parse().map_err(|e| {
            Error::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("failed to parse Cargo.toml: {}", e),
            ))
        })?
    } else {
        // Create new Cargo.toml structure
        let mut doc = DocumentMut::new();

        let mut package = Table::new();
        package.insert("name", value(node_name));
        package.insert("version", value("0.1.0"));
        package.insert("edition", value("2024"));
        doc.insert("package", Item::Table(package));

        doc.insert("dependencies", Item::Table(Table::new()));

        doc
    };

    // Ensure dependencies section exists and add peppygen
    if !doc.contains_key("dependencies") {
        doc.insert("dependencies", Item::Table(Table::new()));
    }

    if let Some(dependencies) = doc.get_mut("dependencies").and_then(|d| d.as_table_mut()) {
        if !dependencies.contains_key("peppygen") {
            let mut peppygen_dep = InlineTable::new();
            peppygen_dep.insert("path", config::consts::PEPPYGEN_OUTPUT_PATH.into());
            dependencies.insert("peppygen", toml_edit::value(peppygen_dep));
        }

        // Strip peppylib — now provided by the precompiled runtime
        dependencies.remove("peppylib");
    }

    fs::write(&cargo_toml_path, doc.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use toml::Value;

    #[test]
    fn ensure_node_cargo_toml_creates_new_file_with_peppygen_dependency() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();

        let cargo_toml_path = node_dir.join("Cargo.toml");
        assert!(!cargo_toml_path.exists());

        ensure_node_cargo_toml(node_dir, "my_test_node").expect("should succeed");

        assert!(cargo_toml_path.exists(), "Cargo.toml should be created");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        let package_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .expect("should have package.name");
        assert_eq!(package_name, "my_test_node");

        let peppygen_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppygen"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppygen dependency with path");
        assert_eq!(peppygen_path, config::consts::PEPPYGEN_OUTPUT_PATH);
    }

    #[test]
    fn ensure_node_cargo_toml_adds_peppygen_to_existing_file() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "existing_node"
            version = "1.0.0"
            edition = "2021"

            [dependencies]
            my-custom-lib = "0.1"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        ensure_node_cargo_toml(node_dir, "existing_node").expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        let package_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .expect("should have package.name");
        assert_eq!(package_name, "existing_node");

        let custom_dep = doc
            .get("dependencies")
            .and_then(|d| d.get("my-custom-lib"))
            .and_then(|s| s.as_str())
            .expect("should have my-custom-lib dependency");
        assert_eq!(custom_dep, "0.1");

        let peppygen_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppygen"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppygen dependency with path");
        assert_eq!(peppygen_path, config::consts::PEPPYGEN_OUTPUT_PATH);
    }

    #[test]
    fn ensure_node_cargo_toml_does_nothing_if_peppygen_already_exists() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "node_with_peppygen"
            version = "2.0.0"
            edition = "2021"

            [dependencies]
            my-custom-lib = "0.1"

            [dependencies.peppygen]
            path = ".peppy/libs/peppygen"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        let content_before = fs::read_to_string(&cargo_toml_path).expect("failed to read");

        ensure_node_cargo_toml(node_dir, "node_with_peppygen").expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        let package_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .expect("should have package.name");
        assert_eq!(package_name, "node_with_peppygen");

        let package_version = doc
            .get("package")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .expect("should have package.version");
        assert_eq!(package_version, "2.0.0");

        let custom_dep = doc
            .get("dependencies")
            .and_then(|d| d.get("my-custom-lib"))
            .and_then(|s| s.as_str())
            .expect("should have my-custom-lib dependency");
        assert_eq!(custom_dep, "0.1");

        let peppygen_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppygen"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppygen dependency with path");
        assert_eq!(peppygen_path, config::consts::PEPPYGEN_OUTPUT_PATH);

        let doc_before: Value = toml::from_str(&content_before).expect("should be valid TOML");
        assert_eq!(doc, doc_before, "logical content should be unchanged");
    }

    #[test]
    fn ensure_node_cargo_toml_strips_peppylib() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "legacy_node"
            version = "1.0.0"
            edition = "2024"

            [dependencies]
            peppylib = { path = ".peppy/libs/peppygen/crates/peppylib" }
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write Cargo.toml");

        ensure_node_cargo_toml(node_dir, "legacy_node").expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        assert!(
            doc.get("dependencies")
                .and_then(|d| d.get("peppylib"))
                .is_none(),
            "peppylib should be stripped from dependencies"
        );
        assert!(
            doc.get("dependencies")
                .and_then(|d| d.get("peppygen"))
                .is_some(),
            "peppygen should be added"
        );
    }

    /// Helper: returns a package name known to be stubbable, or None if the
    /// precompiled manifest has no stubbable crates (shouldn't happen in CI).
    fn any_stubbable_package() -> Option<String> {
        rust::precompiled::stubbable_crates()
            .first()
            .map(|c| c.package_name.clone())
    }

    #[test]
    fn sync_cargo_patches_adds_entries_for_matching_deps() {
        let Some(pkg) = any_stubbable_package() else {
            return; // skip if no stubbable crates
        };

        let temp_dir = TempDir::new().unwrap();
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let content = format!(
            r#"[package]
name = "test_node"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = ".peppy/libs/peppygen" }}
{pkg} = "0.1"
"#
        );
        fs::write(&cargo_toml_path, &content).unwrap();

        sync_cargo_patches(node_dir).unwrap();

        let result = fs::read_to_string(&cargo_toml_path).unwrap();
        let doc: Value = toml::from_str(&result).unwrap();

        let patch_path = doc
            .get("patch")
            .and_then(|p| p.get("crates-io"))
            .and_then(|c| c.get(&pkg))
            .and_then(|e| e.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have patch entry for matching dep");
        assert!(
            patch_path.starts_with(".peppy/patches/"),
            "patch path should point to .peppy/patches/"
        );
    }

    #[test]
    fn sync_cargo_patches_skips_non_precompiled_deps() {
        let temp_dir = TempDir::new().unwrap();
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let content = r#"[package]
name = "test_node"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = { path = ".peppy/libs/peppygen" }
some-unknown-crate = "1.0"
"#;
        fs::write(&cargo_toml_path, content).unwrap();

        sync_cargo_patches(node_dir).unwrap();

        let result = fs::read_to_string(&cargo_toml_path).unwrap();
        let doc: Value = toml::from_str(&result).unwrap();

        assert!(
            doc.get("patch").is_none(),
            "should not add [patch] for non-precompiled deps"
        );
    }

    #[test]
    fn sync_cargo_patches_is_idempotent() {
        let Some(pkg) = any_stubbable_package() else {
            return;
        };

        let temp_dir = TempDir::new().unwrap();
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let content = format!(
            r#"[package]
name = "test_node"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = ".peppy/libs/peppygen" }}
{pkg} = "0.1"
"#
        );
        fs::write(&cargo_toml_path, &content).unwrap();

        sync_cargo_patches(node_dir).unwrap();
        let after_first = fs::read_to_string(&cargo_toml_path).unwrap();

        sync_cargo_patches(node_dir).unwrap();
        let after_second = fs::read_to_string(&cargo_toml_path).unwrap();

        assert_eq!(
            after_first, after_second,
            "second call should be idempotent"
        );
    }

    #[test]
    fn sync_cargo_patches_removes_stale_entries() {
        let Some(pkg) = any_stubbable_package() else {
            return;
        };

        let temp_dir = TempDir::new().unwrap();
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        // First: Cargo.toml with a matching dep → gets patched
        let content = format!(
            r#"[package]
name = "test_node"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = ".peppy/libs/peppygen" }}
{pkg} = "0.1"
"#
        );
        fs::write(&cargo_toml_path, &content).unwrap();
        sync_cargo_patches(node_dir).unwrap();

        let result: Value = toml::from_str(&fs::read_to_string(&cargo_toml_path).unwrap()).unwrap();
        assert!(
            result.get("patch").is_some(),
            "should have patch section with dep present"
        );

        // Now remove the matching dep and re-sync
        let content_no_dep = r#"[package]
name = "test_node"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = { path = ".peppy/libs/peppygen" }
"#;
        fs::write(&cargo_toml_path, content_no_dep).unwrap();
        sync_cargo_patches(node_dir).unwrap();

        let result: Value = toml::from_str(&fs::read_to_string(&cargo_toml_path).unwrap()).unwrap();
        assert!(
            result.get("patch").is_none(),
            "patch section should be removed when no matching deps"
        );
    }
}
