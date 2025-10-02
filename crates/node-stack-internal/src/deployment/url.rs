use super::types::DeploymentMap;
use crate::error::{Error, Result};
use config::peppy_config::Deployment;
use std::path::Path;

pub fn resolve_remote_url(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
    url: String,
) -> Result<DeploymentMap> {
    Err(Error::NotImplemented("remote http deployments"))
}
