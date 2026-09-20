//! Integration tests that parse the example configs shipped inside this crate
//! (`config/examples/`) to ensure the schema types stay in sync with
//! the ground-truth files.
//!
//! If any test here fails, it means the config types in `config` have
//! drifted from the example files — either the code or the examples need updating.
//!
//! The docs-snippet equivalent — parsing the `peppy.json5` files under
//! `peppy/docs/.../guides/snippets` — lives in the `peppy` repo's
//! `docs-integration-tests` crate (`tests/snippet_configs.rs`), where both the
//! docs and a dependency on this crate are available. It is intentionally not
//! duplicated here so this workspace stays self-contained and never reaches back
//! into `peppy/`.

use peppy_config_model::consts::NODE_CONFIG_FILE;
use peppy_config_model::node::NodeConfigParser;
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
