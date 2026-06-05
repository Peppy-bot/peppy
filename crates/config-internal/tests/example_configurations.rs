//! Integration tests that parse every example and documentation snippet config
//! to ensure the schema types stay in sync with the ground-truth files.
//!
//! If any test here fails, it means the config types in `config-internal` have
//! drifted from the example files — either the code or the examples need updating.

use config::consts::NODE_CONFIG_FILE;
use config::interface::PeppyInterfaceParser;
use config::launcher::PeppyLauncherParser;
use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Walk `root` recursively and collect every file named `peppy.json5`.
fn find_node_configs(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|name| name == NODE_CONFIG_FILE)
        })
        .map(|e| e.into_path())
        .collect()
}

/// The schema tag a config declares, read without parsing the whole document.
/// Snippet `peppy.json5` files mix node and interface schemas, so the test must
/// peek the tag and dispatch each file to the parser that matches it.
#[derive(Deserialize)]
struct SchemaPeek {
    peppy_schema: PeppySchema,
}

/// Parse `path` with the typed parser matching its declared `peppy_schema` and
/// assert it succeeds. This keeps interface snippets covered by the same
/// schema-sync guarantee as node snippets instead of skipping them.
fn assert_parses_with_matching_schema(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let peek: SchemaPeek = serde_json5::from_str(&content)
        .unwrap_or_else(|e| panic!("missing peppy_schema in {}: {e}", path.display()));

    match peek.peppy_schema {
        PeppySchema::NodeV1 => {
            let result = NodeConfigParser::from_path(path);
            assert!(
                result.is_ok(),
                "failed to parse node {}: {:?}",
                path.display(),
                result.unwrap_err()
            );
        }
        PeppySchema::InterfaceV1 => {
            let result = PeppyInterfaceParser::from_path(path);
            assert!(
                result.is_ok(),
                "failed to parse interface {}: {:?}",
                path.display(),
                result.unwrap_err()
            );
        }
        PeppySchema::LauncherV1 => panic!(
            "unexpected launcher_v1 among node/interface snippets: {}",
            path.display()
        ),
    }
}

#[test]
fn test_example_node_configs_parse() {
    let examples_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("nodes_example_1");

    assert!(
        examples_root.is_dir(),
        "examples directory not found: {}",
        examples_root.display()
    );

    let configs = find_node_configs(&examples_root);

    assert!(
        configs.len() >= 5,
        "expected at least 5 node configs under {}, found {}",
        examples_root.display(),
        configs.len()
    );

    for path in &configs {
        let result = NodeConfigParser::from_path(path);
        assert!(
            result.is_ok(),
            "failed to parse {}: {:?}",
            path.display(),
            result.unwrap_err()
        );
    }
}

#[test]
fn test_docs_snippet_configs_parse() {
    let snippets_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/src/content/docs/guides/snippets");

    assert!(
        snippets_root.is_dir(),
        "docs snippets directory not found: {}",
        snippets_root.display()
    );

    let configs = find_node_configs(&snippets_root);

    assert!(
        configs.len() >= 9,
        "expected at least 9 snippet configs under {}, found {}",
        snippets_root.display(),
        configs.len()
    );

    for path in &configs {
        assert_parses_with_matching_schema(path);
    }
}

#[test]
fn test_example_launcher_config_parses() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("nodes_example_1")
        .join("peppy_launcher.json5");

    let result = PeppyLauncherParser::from_path(&path);
    assert!(
        result.is_ok(),
        "failed to parse {}: {:?}",
        path.display(),
        result.unwrap_err()
    );

    let launcher = result.unwrap();
    assert!(
        !launcher.deployments.is_empty(),
        "example launcher should contain at least one deployment"
    );
}
