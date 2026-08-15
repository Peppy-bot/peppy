use std::path::PathBuf;
use std::sync::Arc;

use core_node_api::encoding::LauncherOrigin;
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::{PeppyLauncherParser, compose};
use tracing::info;

use super::launch::infer_launcher_origin;
use crate::context::AppContext;
use crate::error::{Error, Result};

/// `peppy stack resolve <name|path> [--with ...]`: print the flat launcher
/// a composed launch would run, and the report of what the selection did.
///
/// Needs no running stack and touches nothing. A filesystem input is read
/// where it stands; a repository name resolves through this machine's
/// launcher cache, exactly as a launch would, minus the goal. The flattened
/// `launcher/v1` document goes to stdout, so it doubles as the escape
/// hatch: flatten, hand-edit, launch the flat file. The resolution report
/// goes to stderr.
pub fn resolve(_ctx: &Arc<AppContext>, launcher_config_path: PathBuf, with: Vec<String>) -> Result<()> {
    let (document, report) = resolve_rendered(launcher_config_path, &with)?;
    for line in report {
        eprintln!("{line}");
    }
    println!("{document}");
    Ok(())
}

/// The resolve command's whole verdict in printable form: the flattened
/// document for stdout and the report lines for stderr. Split from
/// [`resolve`] so a test can read the output instead of capturing stdout.
pub fn resolve_rendered(
    launcher_config_path: PathBuf,
    with: &[String],
) -> Result<(String, Vec<String>)> {
    let path = match infer_launcher_origin(launcher_config_path)? {
        LauncherOrigin::Fs(path) => path,
        LauncherOrigin::Repository { name } => core_node::resolve_repo_launcher_path(
            &name,
            &PeppyDirs::default(),
            &|message: &str| info!("{message}"),
        )
        .map_err(Error::ExecutionFailed)?,
    };

    let parsed = PeppyLauncherParser::from_path(&path).map_err(Error::DaemonConfig)?;
    let (flat, report) =
        compose(&parsed, &path, with).map_err(|e| Error::ExecutionFailed(e.to_string()))?;

    let document = json5_pretty::to_string_pretty(&flat)
        .map_err(|e| Error::ExecutionFailed(format!("cannot serialize the flat launcher: {e}")))?;
    Ok((document, report.render_lines()))
}
