//! Helpers the launcher test binaries share. Each binary uses a subset.
#![allow(dead_code)]

use config::runtime::{CopyName, Name};
use core_node_api::encoding::{LaunchWord, SelectionWord};
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::launcher::{DeploymentTargets, PeppyLauncherParser, PreparedLauncher};
use daemon_config::source::ExposureRef;
use std::fs;
use std::path::{Path, PathBuf};

/// Writes `content` at `path`, creating the directories above it.
pub(crate) fn write(path: &Path, content: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    path.to_path_buf()
}

/// A `launcher_fragment/v1` document around `body`.
pub(crate) fn fragment_file(body: &str) -> String {
    format!("{{ peppy_schema: \"launcher_fragment/v1\", {body} }}")
}

/// `document` held ready to compose, read as the launcher at `fleet.json5`.
pub(crate) fn load(document: &str) -> PreparedLauncher {
    let parsed = PeppyLauncherParser::from_content(document).unwrap();
    PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap()
}

/// The `--with` words of one launch, as the composer takes them.
pub(crate) fn words(entries: &[&str]) -> Vec<LaunchWord> {
    entries.iter().map(|entry| entry.parse().unwrap()).collect()
}

/// The `--with` words of one join, as the composer takes them.
pub(crate) fn join_words(entries: &[&str]) -> Vec<SelectionWord> {
    entries.iter().map(|entry| entry.parse().unwrap()).collect()
}

/// `value` as a validated name.
pub(crate) fn name(value: &str) -> Name {
    Name::new(value).expect("a name")
}

/// `value` as a validated copy name.
pub(crate) fn copy_name(value: &str) -> CopyName {
    CopyName::new(value).expect("a copy name")
}

/// The targets of a built-in MCP deployment of `exposures`, for the test
/// exposures these binaries deploy: `framework_controls` and
/// `copies_controls` serve one `robot_membership` daemon target each,
/// `stack` and `copies`, and `camera` links `front_camera`.
pub(crate) fn exposure_targets(exposures: &[ExposureRef]) -> DeploymentTargets {
    let mut targets = DeploymentTargets::default();
    for exposure in exposures {
        match exposure.name.as_str() {
            "framework_controls" => {
                targets
                    .daemon
                    .insert("stack".to_owned(), DaemonInterface::RobotMembership);
            }
            "copies_controls" => {
                targets
                    .daemon
                    .insert("copies".to_owned(), DaemonInterface::RobotMembership);
            }
            "camera" => {
                targets.linked.insert("front_camera".to_owned());
            }
            other => panic!("no targets known for exposure `{other}`"),
        }
    }
    targets
}
