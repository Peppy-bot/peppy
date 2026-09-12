//! Helpers the launcher test binaries share. Each binary uses a subset.
#![allow(dead_code)]

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

/// The `--with` words of one launch, as the composer takes them.
pub fn words(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| entry.to_string()).collect()
}
