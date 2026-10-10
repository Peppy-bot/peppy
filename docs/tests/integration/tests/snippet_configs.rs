//! Parses every peppy document (`.json5` file) under the docs guide snippets to
//! ensure the user-facing examples stay in sync with the schema types.
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
use daemon_config::launcher::{LauncherFragmentParser, PeppyLauncherParser};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::pairing::PeppyPairingParser;
use daemon_config::repository::PeppyRepositoryIndexParser;
use docs_integration_tests::workspace_root;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets";

/// Walk `root` recursively and collect every peppy document.
///
/// Any `.json5` file, not only `peppy.json5`: a launcher, a launcher fragment,
/// an MCP exposure and a repository index each have a file name of their own.
/// The schema decides how a file is parsed (see
/// [`assert_parses_with_matching_schema`]), so the walk only has to find the
/// files.
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

/// The schema tag a document declares, read without parsing the whole document.
/// The snippets mix documents of several schemas, so the test must peek the tag
/// and dispatch each file to the parser that matches it.
#[derive(Deserialize)]
struct SchemaPeek {
    peppy_schema: PeppySchema,
}

/// Parse `path` with the typed parser matching its declared `peppy_schema` and
/// assert it succeeds, so every documented schema, not only the node manifest,
/// carries the same schema-sync guarantee.
fn assert_parses_with_matching_schema(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let peek: SchemaPeek = serde_json5::from_str(&content)
        .unwrap_or_else(|e| panic!("missing peppy_schema in {}: {e}", path.display()));

    match peek.peppy_schema {
        PeppySchema::NodeV1 => {
            let parsed = NodeConfigParser::from_path(path);
            assert_parsed("node", path, parsed);
        }
        PeppySchema::ContractV1 => {
            let parsed = PeppyContractParser::from_path(path);
            assert_parsed("contract", path, parsed);
        }
        PeppySchema::PairingV1 => {
            let parsed = PeppyPairingParser::from_path(path);
            assert_parsed("pairing", path, parsed);
        }
        PeppySchema::LauncherV1 => {
            let parsed = PeppyLauncherParser::from_path(path);
            assert_parsed("launcher", path, parsed);
        }
        PeppySchema::LauncherFragmentV1 => {
            let parsed = LauncherFragmentParser::from_path(path);
            assert_parsed("launcher fragment", path, parsed);
        }
        PeppySchema::RepositoryV1 => {
            let parsed = PeppyRepositoryIndexParser::from_path(path);
            assert_parsed("repository index", path, parsed);
        }
        PeppySchema::McpExposureV1 => {
            let parsed = PeppyMcpExposureParser::from_path(path);
            assert_parsed("MCP exposure", path, parsed);
        }
    }
}

/// Fail with the document kind, its path and the parser's error, so that a
/// failure names the snippet that drifted from its schema.
fn assert_parsed<T, E: std::fmt::Debug>(kind: &str, path: &Path, parsed: Result<T, E>) {
    if let Err(error) = parsed {
        panic!("failed to parse {kind} {}: {error:?}", path.display());
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
