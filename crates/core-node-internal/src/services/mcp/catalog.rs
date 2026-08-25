//! `peppy mcp catalog <name:tag>`: the catalog the server derives for one
//! exposure, printed on demand.

use super::resolve_exposure_deployment;
use daemon_config::consts::PeppyDirs;
use daemon_config::source::ExposureRef;
use peppy_mcp_catalog::ExposureBundle;

/// Resolves `name:tag` and its contracts through this machine's caches and
/// derives the catalog the built-in server would serve for it: the same
/// derivation, so what this prints is what a running endpoint advertises.
pub fn derive_exposure_catalog(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    on_feedback: &dyn Fn(&str),
) -> Result<ExposureBundle, String> {
    let reference = ExposureRef {
        name: name.to_owned(),
        tag: tag.to_owned(),
    };
    let resolved =
        resolve_exposure_deployment(peppy_dirs, std::slice::from_ref(&reference), on_feedback)?;
    resolved
        .plan
        .bundles
        .into_iter()
        .next()
        .ok_or_else(|| format!("exposure `{reference}` produced no catalog"))
}
