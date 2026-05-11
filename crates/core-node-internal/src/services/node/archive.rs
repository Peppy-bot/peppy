//! Archive handling for node sources: detection of supported extensions
//! (`.tar.zst`/`.tar.zstd`/`.tzst`), extraction of a local archive into
//! a tempdir, and locating the node root inside an already-extracted
//! bundle. `extract_tar_zst` itself is re-exported from `node_stack`.

use config::consts::NODE_CONFIG_FILE;
use config::node::{NodeConfig, NodeConfigParser};
use std::path::{Path, PathBuf};

pub(crate) use node_stack::extract_tar_zst;

pub(crate) struct ResolvedLocalArchiveSource {
    pub(crate) node_config: NodeConfig,
    pub(crate) source_path: PathBuf,
    pub(crate) temp_dir: tempfile::TempDir,
}

fn is_supported_archive_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".tar.zst") || path.ends_with(".tar.zstd") || path.ends_with(".tzst")
}

pub(crate) fn is_supported_fs_archive(path: &Path) -> bool {
    is_supported_archive_path(path.to_string_lossy().as_ref())
}

pub(crate) fn is_supported_http_archive(url: &url::Url) -> bool {
    is_supported_archive_path(url.path())
}

pub(crate) fn locate_node_root_dir(extracted_dir: &Path) -> std::result::Result<PathBuf, String> {
    let direct = extracted_dir.join(NODE_CONFIG_FILE);
    if direct.is_file() {
        return Ok(extracted_dir.to_path_buf());
    }

    let mut candidate_dirs = Vec::new();
    for entry in std::fs::read_dir(extracted_dir).map_err(|e| {
        format!(
            "Failed to list extracted bundle directory {}: {}",
            extracted_dir.display(),
            e
        )
    })? {
        let entry = entry.map_err(|e| {
            format!(
                "Failed to read extracted bundle directory entry in {}: {}",
                extracted_dir.display(),
                e
            )
        })?;
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "Failed to read file type for extracted bundle entry {}: {}",
                entry.path().display(),
                e
            )
        })?;
        if file_type.is_dir() {
            candidate_dirs.push(entry.path());
        }
    }

    let mut matching_dirs: Vec<PathBuf> = candidate_dirs
        .into_iter()
        .filter(|candidate| candidate.join(NODE_CONFIG_FILE).is_file())
        .collect();
    if matching_dirs.len() == 1 {
        return Ok(matching_dirs.pop().expect("matching dir should exist"));
    }

    Err(format!(
        "Bundle does not contain {} at the root (or exactly one top-level folder)",
        NODE_CONFIG_FILE
    ))
}

pub(crate) fn resolve_local_archive_source(
    archive_path: &Path,
) -> std::result::Result<ResolvedLocalArchiveSource, String> {
    let temp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temporary directory: {}", e))?;

    extract_tar_zst(archive_path, temp_dir.path())?;
    let source_path = locate_node_root_dir(temp_dir.path())?;
    let config_path = source_path.join(NODE_CONFIG_FILE);
    let node_config = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;

    Ok(ResolvedLocalArchiveSource {
        node_config,
        source_path,
        temp_dir,
    })
}
