//! `peppy repo index`: write a repository's index, or verify it and, on
//! request, the exposures it publishes.

use std::path::{Path, PathBuf};

use core_node::{check_repository_exposures, check_repository_index, publish_repository_index};
use daemon_config::consts::{PeppyDirs, REPOSITORY_INDEX_FILE};
use tracing::info;

use crate::error::{Error, Result};

/// What `--check` looks at beyond the index itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckScope {
    /// The index against the tree: structural, no caches.
    Index,
    /// The index, then every exposure against the contracts it references,
    /// resolved through the machine's repository caches.
    IndexAndRepositories,
}

pub fn repo_index(path: Option<PathBuf>, check: Option<CheckScope>) -> Result<()> {
    let root = path.unwrap_or_else(|| PathBuf::from("."));
    if !root.is_dir() {
        return Err(Error::ExecutionFailed(format!(
            "not a directory: {}",
            root.display()
        )));
    }
    match check {
        Some(scope) => check_index(&root, scope, &PeppyDirs::default()),
        None => write_index(&root),
    }
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

/// The check, against the caches under `peppy_dirs` when the scope asks
/// for repositories. Every drift and every exposure problem is reported at
/// once; the command fails when there is any.
pub fn check_index(root: &Path, scope: CheckScope, peppy_dirs: &PeppyDirs) -> Result<()> {
    let drifts = check_repository_index(root).map_err(index_failure)?;
    let findings = match scope {
        CheckScope::Index => Vec::new(),
        CheckScope::IndexAndRepositories => {
            check_repository_exposures(root, peppy_dirs, &|message: &str| info!("{message}"))
                .map_err(index_failure)?
        }
    };
    if drifts.is_empty() && findings.is_empty() {
        info!(
            "{} matches the repository{}",
            root.join(REPOSITORY_INDEX_FILE).display(),
            match scope {
                CheckScope::Index => "",
                CheckScope::IndexAndRepositories =>
                    " and every exposure validates against its contracts",
            }
        );
        return Ok(());
    }
    let mut report = String::new();
    if !drifts.is_empty() {
        report.push_str(&format!(
            "{} does not match the repository:{}\n\nRun `peppy repo index {}` and commit the result.",
            root.join(REPOSITORY_INDEX_FILE).display(),
            drifts
                .iter()
                .map(|drift| format!("\n  - {drift}"))
                .collect::<String>(),
            root.display()
        ));
    }
    if !findings.is_empty() {
        if !report.is_empty() {
            report.push_str("\n\n");
        }
        report.push_str(&format!(
            "{} exposure{} do{} not validate:{}",
            findings.len(),
            if findings.len() == 1 { "" } else { "s" },
            if findings.len() == 1 { "es" } else { "" },
            findings
                .iter()
                .map(|finding| format!("\n  - {finding}"))
                .collect::<String>()
        ));
    }
    Err(Error::ExecutionFailed(report))
}

fn index_failure(e: core_node::IndexError) -> Error {
    Error::ExecutionFailed(e.to_string())
}
