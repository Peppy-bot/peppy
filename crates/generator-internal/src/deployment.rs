mod builder;
mod git;
mod local;
mod remote;
mod url;

pub(crate) mod types;

use crate::deployment::types::DeploymentMap;
use crate::error::Result;
use config::{Deployment, DeploymentSource, NodeConfig};
use std::path::Path;

pub use builder::DeploymentMappingBuilder;
pub use local::resolve_local_deployment;
pub use remote::resolve_remote_deployment;

#[derive(Debug, Clone)]
pub struct ResolvedDeploymentNodes {
    deployment_maps: Vec<DeploymentMap>,
}

impl ResolvedDeploymentNodes {
    pub fn new(deployment_maps: Vec<DeploymentMap>) -> Self {
        Self { deployment_maps }
    }

    /// Ensures that every `Deployment` maps to a known node.
    ///
    /// This is ensured by doing the following:
    /// 1. If `Deployment::source` is `DeploymentSource::Local`, look for the node in the provided
    ///    `nodes` vector. The `name` and the version must match; otherwise return `NodeNotFound`.
    /// 2. If `Deployment::source` is `DeploymentSource::Remote`, pull the node from the source (Git or
    ///    `https://nodes.peppy.bot/`) or return `NodeNotFound` if the node cannot be pulled. The `name` of the node
    ///    and `tag` should match; otherwise return `NoMatchingNode`. The pulled nodes are stored inside `<root_dir>/.peppy/nodes`
    /// 3. If `Deployment::source` is `DeploymentSource::Network`, expect another root node on the same
    ///    network to provide it.
    pub fn map(
        nodes_cache_dir: impl AsRef<Path>,
        deployments: &[Deployment],
        nodes: &[NodeConfig],
    ) -> Result<Self> {
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
            .collect::<Result<Vec<_>>>()
            .map(Self::new)
    }

    pub fn deployment_maps(&self) -> &[DeploymentMap] {
        &self.deployment_maps
    }

    pub fn into_deployment_maps(self) -> Vec<DeploymentMap> {
        self.deployment_maps
    }

    /// Once we have a deployment map, ensure nodes subscribe to known message formats.
    pub fn validate_message_formats(self) -> Result<ValidatedDeploymentMessages> {
        // TODO: Enrich deployment maps with message format validation once formats exist.
        let deployment_maps = self.deployment_maps;
        Ok(ValidatedDeploymentMessages::new(deployment_maps))
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedDeploymentMessages {
    deployment_maps: Vec<DeploymentMap>,
}

impl ValidatedDeploymentMessages {
    pub fn new(deployment_maps: Vec<DeploymentMap>) -> Self {
        Self { deployment_maps }
    }

    pub fn deployment_maps(&self) -> &[DeploymentMap] {
        &self.deployment_maps
    }

    pub fn into_deployment_maps(self) -> Vec<DeploymentMap> {
        self.deployment_maps
    }
}
