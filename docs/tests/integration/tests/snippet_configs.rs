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
use daemon_config::daemon_interface::{ProductSheet, RobotMembershipScope};
use daemon_config::launcher::{LauncherFragmentParser, PeppyLauncherParser};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::pairing::PeppyPairingParser;
use daemon_config::repository::PeppyRepositoryIndexParser;
use docs_integration_tests::workspace_root;
use peppy_codec::contract::PeppyContractParser;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const SNIPPETS_ROOT: &str = "docs/src/content/docs/guides/snippets";

/// The launcher snippet whose `robot_membership` scope the launch files guide
/// and the MCP guide show.
const FRAMEWORK_CONTROLS_LAUNCHER: &str = "launchers/mcp_framework_controls.json5";

/// The name of the directories of the product sheets that the launcher
/// snippets include: a product sheet is a plain JSON5 value with no
/// `peppy_schema`.
const PRODUCT_SHEETS_DIR: &str = "products";

/// Walk `root` recursively and collect every `.json5` file.
///
/// Any `.json5` file, not only `peppy.json5`: a launcher, a launcher fragment,
/// an MCP exposure and a repository index each have a file name of their own.
/// The schema decides how a peppy document is parsed (see
/// [`assert_parses_with_matching_schema`]), and a product sheet is parsed as
/// one (see [`assert_parses_as_product_sheet`]), so the walk only has to find
/// the files.
fn find_json5_files(root: &Path) -> Vec<PathBuf> {
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

/// Whether `path` is a product sheet: a file in a [`PRODUCT_SHEETS_DIR`]
/// directory.
fn is_product_sheet(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|directory| directory == PRODUCT_SHEETS_DIR)
}

/// Parse the product sheet at `path`, as the scope of a launcher that
/// includes it parses it.
fn assert_parses_as_product_sheet(path: &Path) {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let parsed = serde_json5::from_str::<ProductSheet>(&content);
    assert_parsed("product sheet", path, parsed);
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

    let (product_sheets, configs): (Vec<PathBuf>, Vec<PathBuf>) = find_json5_files(&snippets_root)
        .into_iter()
        .partition(|path| is_product_sheet(path));

    assert!(
        configs.len() >= 9,
        "expected at least 9 snippet configs under {}, found {}",
        snippets_root.display(),
        configs.len()
    );

    for path in &configs {
        assert_parses_with_matching_schema(path);
    }
    assert!(
        !product_sheets.is_empty(),
        "expected a product sheet under {}",
        snippets_root.display()
    );
    for path in &product_sheets {
        assert_parses_as_product_sheet(path);
    }
}

/// A launcher keeps a scope as a free value, so the launcher parse above does
/// not read it. The scope the docs show parses into the type of its
/// interface, as a launch parses it, with the options and the features the
/// guides describe.
#[test]
fn docs_robot_membership_scope_parses() {
    let path = workspace_root()
        .join(SNIPPETS_ROOT)
        .join(FRAMEWORK_CONTROLS_LAUNCHER);
    let launcher = PeppyLauncherParser::from_path(&path)
        .unwrap_or_else(|error| panic!("failed to parse launcher {}: {error:?}", path.display()));
    let scope = launcher
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .find_map(|instance| instance.daemon_scopes.get("stack"))
        .unwrap_or_else(|| panic!("{} gives no scope to target `stack`", path.display()));

    let scope = RobotMembershipScope::deserialize(scope).unwrap_or_else(|error| {
        panic!(
            "the `robot_membership` scope of {} does not parse: {error}",
            path.display()
        )
    });

    /// A feature of a scoped option: its name, the axis it fills and the
    /// option of that axis that it holds.
    type Feature<'a> = (&'a str, &'a str, &'a str);
    let features: Vec<(&str, Vec<Feature>)> = scope
        .options()
        .iter()
        .map(|entry| {
            (
                entry.option().as_str(),
                entry
                    .features()
                    .iter()
                    .map(|feature| {
                        (
                            feature.feature().as_str(),
                            feature.axis().as_str(),
                            feature.option().as_str(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        features,
        [
            ("openarm_sim", vec![("ai_brain", "brain", "ai_brain_vla")]),
            ("so101_sim", Vec::new()),
        ],
        "the scope offers the options and the features the guides describe"
    );
    let models: Vec<(&str, &str)> = scope
        .options()
        .iter()
        .map(|entry| (entry.option().as_str(), entry.model().as_str()))
        .collect();
    assert_eq!(
        models,
        [("openarm_sim", "openarm_v2"), ("so101_sim", "so101")],
        "the scope names the robot model of each option"
    );
    let so101 = scope.option("so101_sim").expect("the option").product();
    assert_eq!(
        (
            so101.vendor(),
            so101
                .price()
                .map(|price| (price.amount(), price.currency()))
        ),
        (Some("WowRobo Robotics"), Some((299.0, "USD"))),
        "the scope holds the product sheet that the launcher includes"
    );
    let openarm = scope.option("openarm_sim").expect("the option").product();
    assert_eq!(
        (
            openarm.vendor(),
            openarm.purchase_url(),
            openarm.price(),
            openarm.availability(),
            openarm.mass_kg(),
            openarm.height_m()
        ),
        (None, None, None, None, None, None),
        "an option with no product has an empty sheet"
    );
    assert_eq!(scope.max_copies().get(), 4);
}
