pub mod common;
pub(crate) mod naming;
#[cfg(test)]
#[macro_use]
mod test_helpers;
pub mod python;
pub mod rust;
pub mod types;

use crate::error::{Error, Result};
use config::{
    consts::{NODE_CONFIG_FILE, PeppyDirs},
    node::{NodeConfigParser, PeppygenLanguage},
};
use python::PythonGenerator;
use rust::RustGenerator;
use std::{fs, io::ErrorKind, path::Path};
use types::{DeploymentInterface, InterfaceVariant, LanguageGenerator};

/// Generate an interface library for the given language from a node directory.
///
/// This function reads the `peppy.json5` configuration file from the `node_dir`,
/// extracts the exposed interfaces, combines them with the provided consumed interfaces,
/// and generates a library for the specified programming language.
/// The library is generated at `node_dir/.peppy/libs/peppygen`.
///
/// # Arguments
/// * `language` - The language to generate for (Rust or Python)
/// * `node_dir` - Path to the node directory containing `peppy.json5`
/// * `consumed_interfaces` - Consumed interfaces with resolved message formats from dependency nodes
///
/// # Errors
/// Returns an error if:
/// - The `peppy.json5` file is not found in `node_dir`
/// - The configuration file cannot be parsed
/// - Code generation fails
pub fn generate_peppygen_lib(
    language: PeppygenLanguage,
    node_dir: impl AsRef<Path>,
    consumed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
    deploy_mode: common::CrateDeployMode,
) -> Result<()> {
    let node_dir = node_dir.as_ref();
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);

    let peppy_dir = node_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir)?;
    write_if_changed(&peppy_dir.join("git.hash"), git_hash.as_bytes())?;

    if !node_config_path.exists() {
        return Err(Error::NodeNotFound(node_dir.display().to_string()));
    }

    let node_config = NodeConfigParser::from_path(&node_config_path)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

    let mut interfaces = collect_exposed_interfaces(&node_config, consumed_interfaces.len());
    // Add the consumed interfaces with resolved message formats
    interfaces.extend(consumed_interfaces);

    // Create the output directory
    let output_dir = node_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    fs::create_dir_all(&output_dir)?;

    let execution = node_config
        .execution
        .expect("BUG: generate_peppygen_lib called on a config without an execution");

    let result = match language {
        PeppygenLanguage::Rust => {
            let mut rust_generator = RustGenerator::new();
            rust_generator.set_parameters(execution.parameters);
            generate_with_backend(
                rust_generator,
                &interfaces,
                &output_dir,
                peppy_dirs,
                deploy_mode,
            )?;
            // Create or update the node's Cargo.toml with peppygen dependency
            ensure_node_cargo_toml(node_dir, node_config.manifest.name.as_str())?;
            Ok(())
        }
        PeppygenLanguage::Python => {
            let mut python_generator = PythonGenerator::new();
            python_generator.set_parameters(execution.parameters);
            python_generator.set_container(execution.container.is_some());
            generate_with_backend(
                python_generator,
                &interfaces,
                &output_dir,
                peppy_dirs,
                deploy_mode,
            )
        }
    };

    // Lastly generate the codegen fingerprint based on the peppy.json5 config file
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    config::fingerprint::generate_node_config_fingerprint(&node_config_path, &output_dir)?;

    result
}

/// Collects all exposed interfaces from a NodeConfig into DeploymentInterface instances.
fn collect_exposed_interfaces(
    config: &config::node::NodeConfig,
    extra_capacity: usize,
) -> Vec<DeploymentInterface> {
    let mut interfaces =
        Vec::with_capacity(count_exposed_interfaces(config).saturating_add(extra_capacity));

    push_interfaces(
        &mut interfaces,
        config
            .interfaces
            .topics
            .as_ref()
            .and_then(|topics| topics.emits.as_deref()),
        InterfaceVariant::EmittedTopic,
    );
    push_interfaces(
        &mut interfaces,
        config
            .interfaces
            .services
            .as_ref()
            .and_then(|services| services.exposes.as_deref()),
        InterfaceVariant::ExposedService,
    );
    push_interfaces(
        &mut interfaces,
        config
            .interfaces
            .actions
            .as_ref()
            .and_then(|actions| actions.exposes.as_deref()),
        InterfaceVariant::ExposedAction,
    );

    interfaces
}

fn count_exposed_interfaces(config: &config::node::NodeConfig) -> usize {
    config
        .interfaces
        .topics
        .as_ref()
        .and_then(|topics| topics.emits.as_ref())
        .map_or(0, Vec::len)
        + config
            .interfaces
            .services
            .as_ref()
            .and_then(|services| services.exposes.as_ref())
            .map_or(0, Vec::len)
        + config
            .interfaces
            .actions
            .as_ref()
            .and_then(|actions| actions.exposes.as_ref())
            .map_or(0, Vec::len)
}

fn push_interfaces<T, F>(interfaces: &mut Vec<DeploymentInterface>, items: Option<&[T]>, wrap: F)
where
    T: Clone,
    F: Fn(T) -> InterfaceVariant,
{
    let Some(items) = items else {
        return;
    };

    interfaces.extend(
        items
            .iter()
            .cloned()
            .map(wrap)
            .map(DeploymentInterface::new),
    );
}

fn generate_with_backend<B>(
    mut backend: B,
    interfaces: &[DeploymentInterface],
    output_dir: &Path,
    peppy_dirs: &PeppyDirs,
    deploy_mode: common::CrateDeployMode,
) -> Result<()>
where
    B: LanguageGenerator,
{
    interfaces
        .iter()
        .try_for_each(|interface| interface.register_with(&mut backend))?;
    backend.build(output_dir, peppy_dirs, deploy_mode)
}

/// Creates or updates the node's Cargo.toml with the peppygen and peppylib dependencies.
///
/// If the Cargo.toml doesn't exist, it creates a new one with the node name.
/// If it exists, it ensures both dependencies are present while preserving formatting.
fn ensure_node_cargo_toml(node_dir: &Path, node_name: &str) -> Result<()> {
    use toml_edit::{DocumentMut, Item, Table, value};

    let cargo_toml_path = node_dir.join("Cargo.toml");

    let existing_contents = match fs::read_to_string(&cargo_toml_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    let mut doc: DocumentMut = if let Some(contents) = existing_contents.as_deref() {
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

    // Ensure dependencies section exists and add peppygen + peppylib
    if !doc.contains_key("dependencies") {
        doc.insert("dependencies", Item::Table(Table::new()));
    }

    if let Some(dependencies) = doc.get_mut("dependencies").and_then(|d| d.as_table_mut()) {
        set_path_dependency(
            dependencies,
            "peppygen",
            config::consts::PEPPYGEN_OUTPUT_PATH,
        );
        set_path_dependency(
            dependencies,
            "peppylib",
            config::consts::PEPPYLIB_OUTPUT_PATH,
        );
    }

    let rendered = doc.to_string();
    write_if_changed(&cargo_toml_path, rendered.as_bytes())?;

    Ok(())
}

fn set_path_dependency(dependencies: &mut toml_edit::Table, name: &str, path: &str) {
    let mut dependency = toml_edit::InlineTable::new();
    dependency.insert("path", path.into());
    dependencies.insert(name, toml_edit::value(dependency));
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents => Ok(()),
        Ok(_) => {
            fs::write(path, contents)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::write(path, contents)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use toml::Value;

    #[test]
    fn ensure_node_cargo_toml_creates_new_file_with_peppygen_and_peppylib_dependencies() {
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

        let peppylib_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppylib"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppylib dependency with path");
        assert_eq!(peppylib_path, config::consts::PEPPYLIB_OUTPUT_PATH);
    }

    #[test]
    fn ensure_node_cargo_toml_adds_deps_to_existing_file() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "existing_node"
            version = "1.0.0"
            edition = "2021"

            [dependencies]
            serde = "1.0"
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

        let serde_dep = doc
            .get("dependencies")
            .and_then(|d| d.get("serde"))
            .and_then(|s| s.as_str())
            .expect("should have serde dependency");
        assert_eq!(serde_dep, "1.0");

        let peppygen_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppygen"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppygen dependency with path");
        assert_eq!(peppygen_path, config::consts::PEPPYGEN_OUTPUT_PATH);

        let peppylib_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppylib"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppylib dependency with path");
        assert_eq!(peppylib_path, config::consts::PEPPYLIB_OUTPUT_PATH);
    }

    #[test]
    fn ensure_node_cargo_toml_overwrites_stale_peppy_paths() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "node_with_stale_paths"
            version = "2.0.0"
            edition = "2021"

            [dependencies]
            serde = "1.0"

            [dependencies.peppygen]
            path = "old/stale/peppygen"

            [dependencies.peppylib]
            path = "old/stale/peppylib"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        ensure_node_cargo_toml(node_dir, "node_with_stale_paths").expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        let package_name = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .expect("should have package.name");
        assert_eq!(package_name, "node_with_stale_paths");

        let package_version = doc
            .get("package")
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .expect("should have package.version");
        assert_eq!(package_version, "2.0.0");

        let serde_dep = doc
            .get("dependencies")
            .and_then(|d| d.get("serde"))
            .and_then(|s| s.as_str())
            .expect("should have serde dependency");
        assert_eq!(serde_dep, "1.0");

        let peppygen_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppygen"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppygen dependency with path");
        assert_eq!(
            peppygen_path,
            config::consts::PEPPYGEN_OUTPUT_PATH,
            "stale peppygen path should be overwritten"
        );

        let peppylib_path = doc
            .get("dependencies")
            .and_then(|d| d.get("peppylib"))
            .and_then(|p| p.get("path"))
            .and_then(|p| p.as_str())
            .expect("should have peppylib dependency with path");
        assert_eq!(
            peppylib_path,
            config::consts::PEPPYLIB_OUTPUT_PATH,
            "stale peppylib path should be overwritten"
        );
    }
}
