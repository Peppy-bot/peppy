use std::path::Path;

use config::{Deployment, NodeConfig};

use crate::error::Result;

use super::{ResolvedDeploymentNodes, ValidatedDeploymentMessages};

#[derive(Debug)]
pub struct DeploymentMappingBuilder<'a> {
    nodes_cache_dir: &'a Path,
    deployments: &'a [Deployment],
    nodes: &'a [NodeConfig],
}

/// Given a deployment list, finds the corresponding nodes required by
impl<'a> DeploymentMappingBuilder<'a> {
    /// # Arguments
    ///
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`)
    /// * `deployments` - The deployment list
    /// * `nodes` - The list of all known nodes in the current instance
    pub fn new(
        nodes_cache_dir: &'a Path,
        deployments: &'a [Deployment],
        nodes: &'a [NodeConfig],
    ) -> Self {
        Self {
            nodes_cache_dir,
            deployments,
            nodes,
        }
    }

    /// # Errors
    ///
    /// This function will return an `error::Error` if:
    /// - The file specified by `path` does not exist (`ErrorKind::NotFound`).
    pub fn resolve_nodes(self) -> Result<NodeResolutionStage> {
        let resolved =
            ResolvedDeploymentNodes::map(self.nodes_cache_dir, self.deployments, self.nodes)?;
        Ok(NodeResolutionStage { resolved })
    }

    pub fn resolve_and_validate(self) -> Result<ValidatedDeploymentMessages> {
        self.resolve_nodes()?.validate_messages()
    }
}

#[derive(Debug)]
pub struct NodeResolutionStage {
    resolved: ResolvedDeploymentNodes,
}

impl NodeResolutionStage {
    pub fn resolved(&self) -> &ResolvedDeploymentNodes {
        &self.resolved
    }

    pub fn into_resolved(self) -> ResolvedDeploymentNodes {
        self.resolved
    }

    pub fn validate_messages(self) -> Result<ValidatedDeploymentMessages> {
        self.resolved.validate_message_formats()
    }
}
