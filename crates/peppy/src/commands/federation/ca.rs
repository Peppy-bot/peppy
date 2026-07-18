use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;

use crate::error::Result;

pub(super) fn init() -> Result<()> {
    let federation_dir = federation::federation_dir(&PeppyDirs::default());
    federation::ca_init(&federation_dir)?;
    println!(
        "Initialized Peppy fleet CA in {}.",
        federation_dir.display()
    );
    Ok(())
}

pub(super) fn issue(hosts: Vec<String>, out: Option<PathBuf>) -> Result<()> {
    let federation_dir = federation::federation_dir(&PeppyDirs::default());
    let out = out.unwrap_or_else(|| federation_dir.clone());
    federation::issue(&federation_dir, &hosts, &out)?;
    println!("Issued federation identity in {}.", out.display());
    Ok(())
}
