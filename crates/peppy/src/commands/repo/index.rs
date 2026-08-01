use std::path::{Path, PathBuf};

use core_node::{check_repository_index, publish_repository_index};
use daemon_config::consts::REPOSITORY_INDEX_FILE;
use tracing::info;

use crate::error::{Error, Result};

/// `peppy repo index`: write a repository's `peppy_repository.json5`, or
/// verify the committed one against the repository with `--check`.
///
/// Operates on a directory on disk and needs no daemon, so it runs in a hub's
/// CI and on a contributor's branch before anything is merged. That is the
/// point of it: an identity claimed twice is caught by the person who claimed
/// it rather than by every machine that later updates.
pub fn repo_index(path: Option<PathBuf>, check: bool) -> Result<()> {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    if !root.is_dir() {
        return Err(Error::ExecutionFailed(format!(
            "not a directory: {}",
            root.display()
        )));
    }
    if check {
        return check_index(&root);
    }
    write_index(&root)
}

fn write_index(root: &Path) -> Result<()> {
    let count = publish_repository_index(root)
        .map_err(index_failure)?
        .declared_count();
    info!(
        "Indexed {count} item{} into {}",
        if count == 1 { "" } else { "s" },
        root.join(REPOSITORY_INDEX_FILE).display()
    );
    Ok(())
}

fn check_index(root: &Path) -> Result<()> {
    let drifts = check_repository_index(root).map_err(index_failure)?;
    if drifts.is_empty() {
        info!(
            "{} matches the repository",
            root.join(REPOSITORY_INDEX_FILE).display()
        );
        return Ok(());
    }
    let report = drifts
        .iter()
        .map(|drift| format!("\n  - {drift}"))
        .collect::<String>();
    Err(Error::ExecutionFailed(format!(
        "{} does not match the repository:{report}\n\nRun `peppy repo index {}` and commit the result.",
        root.join(REPOSITORY_INDEX_FILE).display(),
        root.display()
    )))
}

fn index_failure(e: core_node::IndexError) -> Error {
    Error::ExecutionFailed(e.to_string())
}
