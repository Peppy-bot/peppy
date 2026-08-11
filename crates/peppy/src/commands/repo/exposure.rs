use std::path::PathBuf;

use core_node::{check_exposure_bundle_file, generate_exposure_bundle_file};
use daemon_config::consts::PeppyDirs;
use tracing::info;

use crate::error::{Error, Result};

/// `peppy repo exposure`: publish an MCP exposure document's bundle file, or
/// verify the committed one with `--check`.
///
/// The exposure's pinned contracts resolve through the local repository
/// caches, so `peppy repo refresh` must have run on this machine. Publishing
/// validates the exposure against exactly the pinned contract bytes and
/// writes `<stem>.bundle.json` next to the document; `--check` regenerates
/// the bundle and refuses a committed file that does not match, byte for
/// byte. Run it in CI so a hub cannot merge a bundle that has drifted from
/// its exposure document.
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
        let drift = check_exposure_bundle_file(&path, &peppy_dirs, &on_feedback)
            .map_err(Error::ExecutionFailed)?;
        return match drift {
            None => {
                info!(
                    "{} matches its exposure document",
                    core_node::exposure_bundle_path(&path).display()
                );
                Ok(())
            }
            Some(drift) => Err(Error::ExecutionFailed(format!(
                "{drift}\n\nRun `peppy repo exposure {}` and commit the result.",
                path.display()
            ))),
        };
    }

    let generated = generate_exposure_bundle_file(&path, &peppy_dirs, &on_feedback)
        .map_err(Error::ExecutionFailed)?;
    info!(
        "Published {} resource{}, {} tool{}, {} task{} to {}",
        generated.bundle.resources.len(),
        plural(generated.bundle.resources.len()),
        generated.bundle.tools.len(),
        plural(generated.bundle.tools.len()),
        generated.bundle.tasks.len(),
        plural(generated.bundle.tasks.len()),
        generated.path.display()
    );
    Ok(())
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
