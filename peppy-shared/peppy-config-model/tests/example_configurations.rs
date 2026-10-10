//! Integration tests that parse the example node configs shipped inside this
//! crate (`examples/nodes_example_1/`), so the config types stay in sync with
//! those files.
//!
//! A failure here means the config types of this crate and the example files
//! disagree: fix the code or the examples.
//!
//! The docs snippets (the `peppy.json5` files under
//! `docs/src/content/docs/guides/snippets`) are parsed by the
//! `docs-integration-tests` crate (`docs/tests/integration/tests/snippet_configs.rs`),
//! which can reach both the docs and this crate. This sealed workspace never
//! reaches outside `peppy-shared/`, so those files are not parsed here.

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
