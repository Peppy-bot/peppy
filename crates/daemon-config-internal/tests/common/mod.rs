//! Helpers the launcher test binaries share. Each binary uses a subset.
#![allow(dead_code)]

use config::runtime::{CopyName, Name};
use core_node_api::encoding::{LaunchWord, SelectionWord};
use daemon_config::launcher::{PeppyLauncherParser, PreparedLauncher};
use std::fs;
use std::path::{Path, PathBuf};

/// Writes `content` at `path`, creating the directories above it.
pub fn write(path: &Path, content: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    path.to_path_buf()
}

/// A `launcher_fragment/v1` document around `body`.
pub fn fragment_file(body: &str) -> String {
    format!("{{ peppy_schema: \"launcher_fragment/v1\", {body} }}")
}

/// `document` held ready to compose, read as the launcher at `fleet.json5`.
pub fn load(document: &str) -> PreparedLauncher {
    let parsed = PeppyLauncherParser::from_content(document).unwrap();
    PreparedLauncher::load(&parsed, Path::new("fleet.json5")).unwrap()
}

/// The `--with` words of one launch, as the composer takes them.
pub fn words(entries: &[&str]) -> Vec<LaunchWord> {
    entries.iter().map(|entry| entry.parse().unwrap()).collect()
}

/// The `--with` words of one join, as the composer takes them.
pub fn join_words(entries: &[&str]) -> Vec<SelectionWord> {
    entries.iter().map(|entry| entry.parse().unwrap()).collect()
}

/// `value` as a validated name.
pub fn name(value: &str) -> Name {
    Name::new(value).expect("a name")
}

/// `value` as a validated copy name.
pub fn copy_name(value: &str) -> CopyName {
    CopyName::new(value).expect("a copy name")
}
