pub mod common;
#[cfg(test)]
mod golden_support;
pub(crate) mod naming;
pub(crate) mod scaffold_tree;
#[cfg(test)]
#[macro_use]
mod test_helpers;
pub mod python;
pub mod rust;
pub(crate) mod testgen;
pub mod types;

use crate::error::{Error, Result};
use config::{
    consts::NODE_CONFIG_FILE,
    node::{NodeConfigParser, PeppygenLanguage},
};
use daemon_config::consts::PeppyDirs;
use python::PythonGenerator;
use rust::RustGenerator;
use std::{fs, io::ErrorKind, path::Path};
use types::{DeploymentInterface, LanguageGenerator};

/// Generate an interface library for the given language from a node directory.
///
/// This function reads the node configuration (from `config_path` if provided,
/// otherwise from `peppy.json5` inside `node_dir`),
/// extracts the exposed interfaces, combines them with the provided consumed interfaces,
/// and generates a library for the specified programming language.
/// The library is generated at `node_dir/.peppy/libs/peppygen`.
///
/// # Arguments
/// * `language` - The language to generate for (Rust or Python)
/// * `node_dir` - Path to the node directory containing `peppy.json5`
/// * `consumed_interfaces` - Consumed interfaces with resolved message formats from dependency nodes
/// * `config_path` - An optional pre-resolved path to the configuration file
///   (`Option<&Path>`). When `Some`, this borrowed path is used directly as the
///   configuration source instead of the default `node_dir/peppy.json5`; the
///   function does **not** canonicalize or otherwise transform the supplied path,
///   so the caller must ensure it is already resolved. When `None`, the function
///   falls back to reading `node_dir/peppy.json5`.
///
/// # Filesystem effects
/// Beyond writing the library at `node_dir/.peppy/libs/peppygen`, this function:
/// - creates `node_dir/.peppy/` and the `peppygen` output directory;
/// - **(Rust only)** creates or updates the node's own `node_dir/Cargo.toml`, adding/overwriting
///   the `peppygen` and `peppylib` path dependencies (overwriting any stale paths), and the
///   `peppy-codec` path dependency when the manifest sets `execution.codec_api: true` (removing
///   it otherwise);
/// - **(Rust only)** deploys five vendored crates (`peppylib`, `peppy-messaging-interface`,
///   `config`, `core-node-api`, `build-helpers`) into `node_dir/.peppy/libs/` (a sibling of
///   `peppygen`), and the two crates of the codec API (`peppy-codec`, `peppy-mcp-catalog`)
///   when the manifest asks for it, either as symlinks into a host-wide shared cache or as
///   physical copies, per `deploy_mode`; Python generation ignores `deploy_mode`;
/// - **(Rust only)** populates that **host-wide shared crate cache** under `peppy_dirs` (using
///   file locking and a per-process staging dir); cache entries are keyed by content-hash +
///   crate version and are not pruned;
/// - **(Python only)** deploys the standalone peppylib Python project (the wrappers and the
///   native extension of the host, or of Linux when the node runs in a container) at
///   `node_dir/.peppy/libs/peppylib`, copied from a lock-protected host-wide cache under
///   `peppy_dirs`, with the module `peppylib.codec` and the Cap'n Proto compiler of the node's
///   platform when the manifest sets `execution.codec_api: true`, and runs ruff over the
///   generated library;
/// - writes the config fingerprint `…/peppygen/peppy.json5.sha256` and, for the node's own
///   source tree, `node_dir/.peppy/git.hash`, but **only after** generation succeeds, so a
///   failed run leaves neither behind.
///
/// `node_tree` says whether `node_dir` is the node's own source tree or a
/// staged copy (see [`common::NodeTree`]). A staged copy gets no `git.hash`:
/// `git_hash` names the peppy build, not the node's sources, and the daemon
/// fingerprints the staged tree to reuse build artifacts, so writing it there
/// would rebuild every node once per peppy commit. The Python fixtures
/// harness also reads `node_tree` to decide whether to bake the tree's path.
///
/// # Errors
/// Returns an error if:
/// - The configuration file (either `config_path` or the default
///   `node_dir/peppy.json5`) does not exist
/// - The configuration file cannot be parsed
/// - Code generation fails
#[allow(clippy::too_many_arguments)]
pub fn generate_peppygen_lib(
    language: PeppygenLanguage,
    node_dir: impl AsRef<Path>,
    consumed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
    deploy_mode: common::CrateDeployMode,
    config_path: Option<&Path>,
    node_tree: common::NodeTree,
) -> Result<()> {
    let node_dir = node_dir.as_ref();
    let node_config_path = config_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| node_dir.join(NODE_CONFIG_FILE));

    let peppy_dir = node_dir.join(daemon_config::consts::PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir)?;
    if !node_config_path.exists() {
        return Err(Error::NodeNotFound(node_config_path.display().to_string()));
    }

    let node_config = NodeConfigParser::from_path(&node_config_path)?;

    let mut interfaces = collect_exposed_interfaces(&node_config, consumed_interfaces.len());
    // Add the consumed interfaces with resolved message formats
    interfaces.extend(consumed_interfaces);

    // Create the output directory
    let output_dir = node_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    fs::create_dir_all(&output_dir)?;

    let execution = node_config.execution;
    let codec_api = common::CodecApi::of_manifest(execution.codec_api);

    match language {
        PeppygenLanguage::Rust => {
            let mut rust_generator = RustGenerator::new();
            rust_generator.set_parameters(execution.parameters);
            rust_generator.set_node_identity(
                node_config.manifest.name.as_str(),
                node_config.manifest.tag.as_str(),
            );
            rust_generator.set_codec_api(codec_api);
            generate_with_backend(
                rust_generator,
                &interfaces,
                &output_dir,
                peppy_dirs,
                deploy_mode,
            )?;
            // Create or update the node's Cargo.toml with peppygen dependency
            ensure_node_cargo_toml(node_dir, node_config.manifest.name.as_str(), codec_api)?;
        }
        PeppygenLanguage::Python => {
            let mut python_generator = PythonGenerator::new();
            python_generator.set_parameters(execution.parameters);
            python_generator.set_container(execution.container.is_some());
            python_generator.set_node_tree(node_tree);
            python_generator.set_node_identity(
                node_config.manifest.name.as_str(),
                node_config.manifest.tag.as_str(),
            );
            python_generator.set_codec_api(codec_api);
            generate_with_backend(
                python_generator,
                &interfaces,
                &output_dir,
                peppy_dirs,
                deploy_mode,
            )?;
        }
    }

    // Only write git.hash and the fingerprint after successful generation.
    if node_tree == common::NodeTree::Source {
        write_if_changed(&peppy_dir.join("git.hash"), git_hash.as_bytes())?;
    }
    config::fingerprint::generate_node_config_fingerprint(&node_config_path, &output_dir)?;
    Ok(())
}

/// Collects the node's own NATIVE exposed interfaces from a NodeConfig into
/// DeploymentInterface instances. Contract-backed entries are skipped here:
/// their shapes live in the contract documents, which the daemon resolves
/// (with the Tier B coverage check) into the pre-resolved interface vec this
/// function's output is merged with.
fn collect_exposed_interfaces(
    config: &config::node::NodeConfig,
    extra_capacity: usize,
) -> Vec<DeploymentInterface> {
    let mut interfaces = Vec::with_capacity(extra_capacity);
    interfaces.extend(
        config
            .interfaces
            .native_emits()
            .map(|topic| DeploymentInterface::emitted_topic(topic.clone(), None)),
    );
    interfaces.extend(
        config
            .interfaces
            .native_service_exposes()
            .map(|service| DeploymentInterface::exposed_service(service.clone(), None)),
    );
    interfaces.extend(
        config
            .interfaces
            .native_action_exposes()
            .map(|action| DeploymentInterface::exposed_action(action.clone(), None)),
    );
    interfaces
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

/// Creates or updates the node's Cargo.toml with the peppygen and peppylib dependencies,
/// and the peppy-codec dependency of a node that asks for the codec API.
///
/// If the Cargo.toml doesn't exist, it creates a new one with the node name.
/// If it exists, it ensures the dependencies are present while preserving formatting, and
/// removes the peppy-codec dependency of a node that does not ask for the codec API.
fn ensure_node_cargo_toml(
    node_dir: &Path,
    node_name: &str,
    codec_api: common::CodecApi,
) -> Result<()> {
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

        doc
    };

    let dependencies = dependency_table(&mut doc, "dependencies")?;
    set_path_dependency(
        dependencies,
        "peppygen",
        config::consts::PEPPYGEN_OUTPUT_PATH,
    );
    set_path_dependency(
        dependencies,
        "peppylib",
        daemon_config::consts::PEPPYLIB_OUTPUT_PATH,
    );
    match codec_api {
        common::CodecApi::Asked => set_path_dependency(
            dependencies,
            PEPPY_CODEC_CRATE,
            daemon_config::consts::PEPPY_CODEC_OUTPUT_PATH,
        ),
        common::CodecApi::NotAsked => {
            dependencies.remove(PEPPY_CODEC_CRATE);
        }
    }

    // The generated test surfaces (`peppygen::mock` / `peppygen::fixtures`)
    // compile only under peppygen's `testing` feature, enabled from the
    // node's dev-dependencies. A plain `cargo build` resolves no top-level
    // dev-dependency, so it leaves the feature off.
    let dev_dependencies = dependency_table(&mut doc, "dev-dependencies")?;
    set_path_dependency_with_features(
        dev_dependencies,
        "peppygen",
        config::consts::PEPPYGEN_OUTPUT_PATH,
        &["testing"],
    );

    let rendered = doc.to_string();
    write_if_changed(&cargo_toml_path, rendered.as_bytes())?;

    Ok(())
}

/// The name of the crate of the codec API in the Cargo.toml of a node.
const PEPPY_CODEC_CRATE: &str = "peppy-codec";

/// The `key` dependency table of a Cargo.toml, appended empty when absent.
/// Cargo accepts a standard table or an inline one. Any other value cannot
/// hold the peppy dependencies, and a Cargo.toml written without them only
/// fails later with an unresolved crate, so it is refused here.
fn dependency_table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    key: &str,
) -> Result<&'a mut dyn toml_edit::TableLike> {
    doc.entry(key)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("`{key}` in Cargo.toml is not a table"),
            ))
        })
}

fn set_path_dependency(dependencies: &mut dyn toml_edit::TableLike, name: &str, path: &str) {
    let mut dependency = toml_edit::InlineTable::new();
    dependency.insert("path", path.into());
    dependencies.insert(name, toml_edit::value(dependency));
}

fn set_path_dependency_with_features(
    dependencies: &mut dyn toml_edit::TableLike,
    name: &str,
    path: &str,
    features: &[&str],
) {
    let mut dependency = toml_edit::InlineTable::new();
    dependency.insert("path", path.into());
    let features: toml_edit::Array = features.iter().copied().collect();
    dependency.insert("features", toml_edit::Value::Array(features));
    dependencies.insert(name, toml_edit::value(dependency));
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    fs::write(path, contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use toml::Value;

    /// The string at `keys` in `doc`, walked one table key at a time.
    fn string_at<'a>(doc: &'a Value, keys: &[&str]) -> &'a str {
        let mut value = doc;
        for key in keys {
            value = value
                .get(key)
                .unwrap_or_else(|| panic!("should have {}", keys.join(".")));
        }
        value
            .as_str()
            .unwrap_or_else(|| panic!("{} should be a string", keys.join(".")))
    }

    /// Asserts the peppygen and peppylib path dependencies.
    fn assert_peppy_path_dependencies(doc: &Value) {
        assert_eq!(
            string_at(doc, &["dependencies", "peppygen", "path"]),
            config::consts::PEPPYGEN_OUTPUT_PATH
        );
        assert_eq!(
            string_at(doc, &["dependencies", "peppylib", "path"]),
            daemon_config::consts::PEPPYLIB_OUTPUT_PATH
        );
    }

    /// Asserts the dev-dependency the testing feature rides in on.
    fn assert_testing_dev_dependency(doc: &Value) {
        let dev = doc
            .get("dev-dependencies")
            .and_then(|d| d.get("peppygen"))
            .expect("should have a peppygen dev-dependency");
        assert_eq!(
            dev.get("path").and_then(|p| p.as_str()),
            Some(config::consts::PEPPYGEN_OUTPUT_PATH),
        );
        let features: Vec<&str> = dev
            .get("features")
            .and_then(|f| f.as_array())
            .expect("dev-dependency should carry features")
            .iter()
            .filter_map(|f| f.as_str())
            .collect();
        assert_eq!(features, ["testing"]);
    }

    #[test]
    fn ensure_node_cargo_toml_creates_new_file_with_peppygen_and_peppylib_dependencies() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();

        let cargo_toml_path = node_dir.join("Cargo.toml");
        assert!(!cargo_toml_path.exists());

        ensure_node_cargo_toml(node_dir, "my_test_node", common::CodecApi::NotAsked)
            .expect("should succeed");

        assert!(cargo_toml_path.exists(), "Cargo.toml should be created");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        assert_eq!(string_at(&doc, &["package", "name"]), "my_test_node");

        assert_peppy_path_dependencies(&doc);

        assert_testing_dev_dependency(&doc);
    }

    #[test]
    fn ensure_node_cargo_toml_adds_deps_to_existing_file() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            [package]
            name = "existing_node"
            version = "0.1.0"
            edition = "2021"

            [dependencies]
            serde = "1.0"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        ensure_node_cargo_toml(node_dir, "existing_node", common::CodecApi::NotAsked)
            .expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        assert_eq!(string_at(&doc, &["package", "name"]), "existing_node");

        assert_eq!(string_at(&doc, &["dependencies", "serde"]), "1.0");

        assert_peppy_path_dependencies(&doc);

        assert_testing_dev_dependency(&doc);
    }

    /// `dependencies` written as an inline table ahead of `[package]` is
    /// valid TOML and valid Cargo, so the peppy dependencies go into it.
    #[test]
    fn ensure_node_cargo_toml_adds_deps_to_an_inline_dependencies_table() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            dependencies = { serde = "1.0" }

            [package]
            name = "inline_node"
            version = "0.1.0"
            edition = "2021"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        ensure_node_cargo_toml(node_dir, "inline_node", common::CodecApi::NotAsked)
            .expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");
        let dependencies = doc.get("dependencies").expect("should keep dependencies");
        assert_eq!(
            dependencies.get("serde").and_then(|s| s.as_str()),
            Some("1.0"),
            "the existing dependency must be kept"
        );
        assert_peppy_path_dependencies(&doc);
        assert_testing_dev_dependency(&doc);
    }

    /// A `dependencies` key that holds no table cannot take the peppy
    /// dependencies, so generation fails instead of writing a Cargo.toml
    /// without them.
    #[test]
    fn ensure_node_cargo_toml_refuses_a_dependencies_value_that_is_not_a_table() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");

        let existing_content = r#"
            dependencies = "serde"

            [package]
            name = "broken_node"
            version = "0.1.0"
            edition = "2021"
        "#;
        fs::write(&cargo_toml_path, existing_content).expect("failed to write existing Cargo.toml");

        let error = ensure_node_cargo_toml(node_dir, "broken_node", common::CodecApi::NotAsked)
            .expect_err("a non-table dependencies value must be refused");
        assert!(
            error
                .to_string()
                .contains("`dependencies` in Cargo.toml is not a table"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml"),
            existing_content,
            "a refused Cargo.toml must be left untouched"
        );
    }

    /// A node that asks for the codec API gets a path dependency on
    /// `peppy-codec`; a node that stops asking loses it, and its other
    /// dependencies stay.
    #[test]
    fn ensure_node_cargo_toml_gives_peppy_codec_only_to_a_node_that_asks() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path();
        let cargo_toml_path = node_dir.join("Cargo.toml");
        let read = || -> Value {
            toml::from_str(&fs::read_to_string(&cargo_toml_path).expect("readable"))
                .expect("should be valid TOML")
        };

        ensure_node_cargo_toml(node_dir, "codec_node", common::CodecApi::Asked)
            .expect("should succeed");
        let asking = read();
        assert_eq!(
            string_at(&asking, &["dependencies", "peppy-codec", "path"]),
            daemon_config::consts::PEPPY_CODEC_OUTPUT_PATH
        );
        assert_peppy_path_dependencies(&asking);

        ensure_node_cargo_toml(node_dir, "codec_node", common::CodecApi::NotAsked)
            .expect("should succeed");
        let silent = read();
        assert!(
            silent["dependencies"].get("peppy-codec").is_none(),
            "a node that does not ask has no peppy-codec"
        );
        assert_peppy_path_dependencies(&silent);
        assert_testing_dev_dependency(&silent);
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

        ensure_node_cargo_toml(
            node_dir,
            "node_with_stale_paths",
            common::CodecApi::NotAsked,
        )
        .expect("should succeed");

        let contents = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
        let doc: Value = toml::from_str(&contents).expect("should be valid TOML");

        assert_eq!(
            string_at(&doc, &["package", "name"]),
            "node_with_stale_paths"
        );

        assert_eq!(string_at(&doc, &["package", "version"]), "2.0.0");

        assert_eq!(string_at(&doc, &["dependencies", "serde"]), "1.0");

        assert_eq!(
            string_at(&doc, &["dependencies", "peppygen", "path"]),
            config::consts::PEPPYGEN_OUTPUT_PATH,
            "stale peppygen path should be overwritten"
        );
        assert_eq!(
            string_at(&doc, &["dependencies", "peppylib", "path"]),
            daemon_config::consts::PEPPYLIB_OUTPUT_PATH,
            "stale peppylib path should be overwritten"
        );

        assert_testing_dev_dependency(&doc);
    }

    #[test]
    fn fingerprint_uses_resolved_config_path() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path().join("node");
        fs::create_dir_all(&node_dir).unwrap();

        // Write a canonical peppy.json5 at the default location
        let canonical_config = r#"{
          peppy_schema: "node/v1",
          manifest: { name: "canonical_node", tag: "v1", description: ["A test node."] },
          execution: { language: "rust", run_cmd: ["./target/release/canonical_node"] }
        }"#;
        fs::write(node_dir.join(NODE_CONFIG_FILE), canonical_config).unwrap();

        // Write a different config at a custom path
        let custom_config = r#"{
          peppy_schema: "node/v1",
          manifest: { name: "custom_node", tag: "v2", description: ["A test node."] },
          execution: { language: "rust", run_cmd: ["./target/release/custom_node"] }
        }"#;
        let custom_path = temp_dir.path().join("custom_peppy.json5");
        fs::write(&custom_path, custom_config).unwrap();

        let peppy_dirs = daemon_config::consts::PeppyDirs::default();
        generate_peppygen_lib(
            config::node::PeppygenLanguage::Rust,
            &node_dir,
            Vec::new(),
            "test-hash",
            &peppy_dirs,
            common::CrateDeployMode::default(),
            Some(custom_path.as_path()),
            common::NodeTree::Source,
        )
        .expect("generation should succeed");

        // Read the fingerprint file directly from the output directory
        let fingerprint_path = node_dir
            .join(config::consts::PEPPYGEN_OUTPUT_PATH)
            .join("peppy.json5.sha256");
        let written_fingerprint = fs::read_to_string(&fingerprint_path)
            .expect("fingerprint file should exist")
            .trim()
            .to_string();

        let expected = config::fingerprint::fingerprint_for_bytes(custom_config.as_bytes());
        let not_expected = config::fingerprint::fingerprint_for_bytes(canonical_config.as_bytes());

        assert_eq!(
            written_fingerprint, expected,
            "fingerprint should match the custom config content"
        );
        assert_ne!(
            written_fingerprint, not_expected,
            "fingerprint should NOT match the canonical config content"
        );

        let git_hash_path = node_dir
            .join(daemon_config::consts::PEPPY_OUTPUT_DIR)
            .join("git.hash");
        let written_hash = fs::read_to_string(&git_hash_path)
            .expect("git.hash file should exist after successful generation");
        assert_eq!(
            written_hash, "test-hash",
            "git.hash should contain the provided hash"
        );
    }

    #[test]
    fn no_fingerprint_written_on_generation_failure() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path().join("node");
        fs::create_dir_all(&node_dir).unwrap();

        // Do NOT write any peppy.json5; generation should fail with NodeNotFound
        let peppy_dirs = daemon_config::consts::PeppyDirs::default();
        let result = generate_peppygen_lib(
            config::node::PeppygenLanguage::Rust,
            &node_dir,
            Vec::new(),
            "test-hash",
            &peppy_dirs,
            common::CrateDeployMode::default(),
            None,
            common::NodeTree::Source,
        );

        assert!(
            result.is_err(),
            "generation should fail when config is missing"
        );

        let fingerprint_path = node_dir
            .join(config::consts::PEPPYGEN_OUTPUT_PATH)
            .join("peppy.json5.sha256");
        assert!(
            !fingerprint_path.exists(),
            "fingerprint file should not exist when generation fails"
        );

        let git_hash_path = node_dir
            .join(daemon_config::consts::PEPPY_OUTPUT_DIR)
            .join("git.hash");
        assert!(
            !git_hash_path.exists(),
            "git.hash file should not exist when generation fails"
        );
    }

    #[test]
    fn staged_generation_writes_no_git_hash() {
        let temp_dir = TempDir::new().expect("failed to create temp directory");
        let node_dir = temp_dir.path().join("node");
        fs::create_dir_all(&node_dir).unwrap();
        fs::write(
            node_dir.join(NODE_CONFIG_FILE),
            r#"{
              peppy_schema: "node/v1",
              manifest: { name: "staged_node", tag: "v1", description: ["A test node."] },
              execution: { language: "rust", run_cmd: ["./target/release/staged_node"] }
            }"#,
        )
        .unwrap();

        let peppy_dirs = daemon_config::consts::PeppyDirs::default();
        generate_peppygen_lib(
            config::node::PeppygenLanguage::Rust,
            &node_dir,
            Vec::new(),
            "daemon-build-hash",
            &peppy_dirs,
            common::CrateDeployMode::default(),
            None,
            common::NodeTree::Staged,
        )
        .expect("generation should succeed");

        let peppy_dir = node_dir.join(daemon_config::consts::PEPPY_OUTPUT_DIR);
        assert!(
            !peppy_dir.join("git.hash").exists(),
            "a staged tree must not record the peppy build hash: it is not a node source"
        );
        assert!(
            node_dir
                .join(config::consts::PEPPYGEN_OUTPUT_PATH)
                .join("peppy.json5.sha256")
                .exists(),
            "the config fingerprint is still written for a staged tree"
        );
    }
}
