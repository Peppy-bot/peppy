use crate::consts::PEPPY_CONFIG_FILE;
use std::path::{Path, PathBuf};

/// Finds the `PEPPY_CONFIG_FILE` recursively starting at `from_dir`
pub fn find_peppy_nodes_from_dir(from_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut peppy_files = Vec::new();
    let from_dir = from_dir.as_ref();

    if !from_dir.is_dir() {
        return peppy_files;
    }

    let walker = walkdir::WalkDir::new(from_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok());

    for entry in walker {
        let path = entry.path();
        if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE)) {
            peppy_files.push(path.to_path_buf());
        }
    }

    peppy_files
}
