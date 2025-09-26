use super::{
    git::resolve_remote_git,
    types::{DeploymentMap, RemoteSpec},
    url::resolve_remote_url,
};
use crate::error::{Error, Result};
use config::Deployment;
use std::path::Path;

pub fn resolve_remote_deployment(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
) -> Result<DeploymentMap> {
    let remote_spec = RemoteSpec::from_node_source(deployment.source.as_ref())
        .ok_or_else(|| Error::NodeNotFound(deployment.name.clone()))?;

    match remote_spec {
        RemoteSpec::Git(spec) => resolve_remote_git(nodes_cache_dir, deployment, spec),
        RemoteSpec::Http(url) => resolve_remote_url(nodes_cache_dir, deployment, url),
    }
}
