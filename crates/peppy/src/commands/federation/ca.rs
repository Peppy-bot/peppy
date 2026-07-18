use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;

use crate::error::{Error, Result};

pub(super) fn init(peppy_dirs: Option<PeppyDirs>) -> Result<()> {
    let dirs = peppy_dirs.unwrap_or_default();
    let federation_dir = federation::federation_dir(&dirs);
    federation::ca_init(&federation_dir)
        .map_err(|error| Error::ExecutionFailed(error.to_string()))?;
    println!(
        "Initialized Peppy fleet CA in {}.",
        federation_dir.display()
    );
    Ok(())
}

pub(super) fn issue(
    hosts: Vec<String>,
    out: Option<PathBuf>,
    peppy_dirs: Option<PeppyDirs>,
) -> Result<()> {
    let dirs = peppy_dirs.unwrap_or_default();
    let federation_dir = federation::federation_dir(&dirs);
    let out = out.unwrap_or_else(|| federation_dir.clone());
    federation::issue(&federation_dir, &hosts, &out)
        .map_err(|error| Error::ExecutionFailed(error.to_string()))?;
    println!("Issued federation identity in {}.", out.display());
    Ok(())
}
