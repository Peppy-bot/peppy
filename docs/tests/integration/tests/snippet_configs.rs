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

use config::node::NodeConfigParser;
use config::schema::PeppySchema;
use daemon_config::contract::PeppyContractParser;
use docs_integration_tests::workspace_root;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets";

/// Walk `root` recursively and collect every peppy document.
///
/// Any `.json5` file, not just `peppy.json5`: a launcher, a contract, and a
/// pairing all have their own conventional names, and filtering on the node
/// filename silently excluded every one of them. The schema is what decides how
/// a file is parsed (see [`assert_parses_with_matching_schema`]), so the walk
/// only has to find the files.
fn find_peppy_documents(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "json5")
        })
        .map(|e| e.into_path())
        .collect()
}

/// The schema tag a config declares, read without parsing the whole document.
/// Snippet `peppy.json5` files mix node and contract schemas, so the test must
/// peek the tag and dispatch each file to the parser that matches it.
#[derive(Deserialize)]
struct SchemaPeek {
    peppy_schema: PeppySchema,
}

/// Parse `path` with the typed parser matching its declared `peppy_schema` and
/// assert it succeeds. This keeps contract snippets covered by the same
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
        PeppySchema::ContractV1 => {
            let result = PeppyContractParser::from_path(path);
            assert!(
                result.is_ok(),
                "failed to parse contract {}: {:?}",
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
        PeppySchema::LauncherV1 => {
            // The federation guide's worked launcher is the headline example
            // of a whole feature. Before this arm parsed it, it was the one
            // piece of documented syntax nothing checked.
            let result = daemon_config::launcher::PeppyLauncherParser::from_path(path);
            assert!(
                result.is_ok(),
                "failed to parse launcher {}: {:?}",
                path.display(),
                result.unwrap_err()
            );
        }
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

    let configs = find_peppy_documents(&snippets_root);

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
