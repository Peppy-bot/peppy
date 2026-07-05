//! Parses every `peppy.json5` snippet under the docs guides to ensure the
//! user-facing examples stay in sync with the `config` crate's schema types.
//!
//! The snippets are the ground truth shown in the docs, so this test lives here
//! (rather than in the `config` crate): the docs ship in this repo while `config`
//! is just a dependency. Keeping it here lets `peppy-shared` stay self-contained
//! instead of reaching back into `peppy/` to find the snippets.
//!
//! If this fails, the config types in `config` have drifted from the documented
//! snippets; either the code or the snippets need updating.

use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use daemon_config::interface::PeppyInterfaceParser;
use docs_integration_tests::workspace_root;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets";

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
        PeppySchema::PairingV1 => {
            let result = daemon_config::pairing::PeppyPairingParser::from_path(path);
            assert!(
                result.is_ok(),
                "failed to parse pairing {}: {:?}",
                path.display(),
                result.unwrap_err()
            );
        }
        PeppySchema::LauncherV1 => panic!(
            "unexpected launcher/v1 among node/interface/pairing snippets: {}",
            path.display()
        ),
    }
}

#[test]
fn docs_snippet_configs_parse() {
    let snippets_root = workspace_root().join(SNIPPETS_ROOT);

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
