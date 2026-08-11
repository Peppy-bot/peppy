use std::path::PathBuf;

use core_node::{check_exposure, publish_exposure};
use daemon_config::consts::PeppyDirs;
use tracing::info;

use crate::error::{Error, Result};

/// `peppy repo exposure`: publish an MCP exposure document's artifacts (the
/// bundle file and the generated MCP server node), or verify the committed
/// ones with `--check`.
///
/// The exposure's pinned contracts resolve through the local repository
/// caches, so `peppy repo refresh` must have run on this machine. Publishing
/// validates the exposure against exactly the pinned contract bytes, writes
/// `<stem>.bundle.json` next to the document, and generates the node into a
/// sibling directory named after it. Exposures selecting actions publish the
/// bundle only: the node generator does not support action-backed tasks yet.
/// `--check` regenerates everything and refuses committed files that do not
/// match, byte for byte. Run it in CI so a hub cannot merge artifacts that
/// have drifted from their exposure document.
pub fn repo_exposure(path: PathBuf, check: bool) -> Result<()> {
    if !path.is_file() {
        return Err(Error::ExecutionFailed(format!(
            "not a file: {}",
            path.display()
        )));
    }
    let peppy_dirs = PeppyDirs::default();
    let on_feedback = |message: &str| info!("{message}");

    if check {
        let drifts =
            check_exposure(&path, &peppy_dirs, &on_feedback).map_err(Error::ExecutionFailed)?;
        if drifts.is_empty() {
            info!(
                "the committed artifacts of {} match its exposure document",
                path.display()
            );
            return Ok(());
        }
        let listed: Vec<String> = drifts.iter().map(|drift| format!("  - {drift}")).collect();
        return Err(Error::ExecutionFailed(format!(
            "{}\n\nRun `peppy repo exposure {}` and commit the result.",
            listed.join("\n"),
            path.display()
        )));
    }

    let published =
        publish_exposure(&path, &peppy_dirs, &on_feedback).map_err(Error::ExecutionFailed)?;
    info!(
        "Published {} resource{}, {} tool{}, {} task{} to {}",
        published.bundle.resources.len(),
        plural(published.bundle.resources.len()),
        published.bundle.tools.len(),
        plural(published.bundle.tools.len()),
        published.bundle.tasks.len(),
        plural(published.bundle.tasks.len()),
        published.bundle_path.display()
    );
    if let Some(node_dir) = &published.node_dir {
        info!(
            "Generated the MCP server node `{}:{}` ({} file{}) at {}",
            published.bundle.node.name,
            published.bundle.node.tag,
            published.node_file_count,
            plural(published.node_file_count),
            node_dir.display()
        );
    }
    Ok(())
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
