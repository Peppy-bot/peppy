mod git;
mod local;
mod remote;
mod url;

pub(crate) mod types;

use crate::deployment::types::DeploymentMap;
use crate::error::Result;
use config::{Deployment, DeploymentSource, NodeConfig};
use std::path::Path;

pub use local::resolve_local_deployment;
pub use remote::resolve_remote_deployment;

/// Ensures that every Deployment maps to a known node.
/// This is ensured by doing the following:
/// 1. If `Deployment::source` is `DeploymentSource::Local`, look for the node in the provided
///    `nodes` vector. The `name` and the version must match; otherwise return `NodeNotFound`.
/// 2. If `Deployment::source` is `DeploymentSource::Remote`, pull the node from the source (Git or
///    `https://nodes.peppy.bot/`) or return `NodeNotFound` if the node cannot be pulled. The `name` of the node
///    and `tag` should match; otherwise return `NoMatchingNode`. The pulled nodes are stored inside `<root_dir>/.peppy/nodes`
/// 3. If `Deployment::source` is `DeploymentSource::Network`, expect another root node on the same
///    network to provide it.
pub fn map_deployment_nodes(
    nodes_cache_dir: impl AsRef<Path>,
    deployments: &[Deployment],
    nodes: &[NodeConfig],
) -> Result<Vec<DeploymentMap>> {
    deployments
        .iter()
        .map(|deployment| match &deployment.source {
            DeploymentSource::Local => resolve_local_deployment(deployment, nodes),
            DeploymentSource::Remote(source) => {
                resolve_remote_deployment(nodes_cache_dir.as_ref(), deployment, source)
            }
            DeploymentSource::Network => {
                todo!("handle network deployment sources")
            }
        })
        .collect()
}

/// Once we have a deployment map, we need to ensure nodes `subscribes_to` topic/services/actions of those nodes can map to
// TODO: Use type state pattern
pub fn map_deployment_nodes_messages_format(_deployment_maps: &[DeploymentMap]) {}
