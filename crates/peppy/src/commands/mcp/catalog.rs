use crate::error::{Error, Result};
use daemon_config::consts::PeppyDirs;
use daemon_config::source::ExposureRef;
use tracing::info;

/// `peppy mcp catalog <name:tag>`: prints the derived catalog as JSON.
pub(super) fn mcp_catalog(exposure: &str) -> Result<()> {
    let rendered = mcp_catalog_rendered(&PeppyDirs::default(), exposure)?;
    println!("{rendered}");
    Ok(())
}

/// The catalog as the command prints it, for a caller that wants the text
/// rather than stdout, and against the Peppy home it names.
pub fn mcp_catalog_rendered(peppy_dirs: &PeppyDirs, exposure: &str) -> Result<String> {
    let reference = ExposureRef::parse(exposure).map_err(Error::ExecutionFailed)?;
    let bundle = core_node::derive_exposure_catalog(
        peppy_dirs,
        &reference.name,
        &reference.tag,
        &|message: &str| info!("{message}"),
    )
    .map_err(Error::ExecutionFailed)?;
    Ok(bundle.to_json_string())
}
