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

impl<'a> DeploymentMappingBuilder<'a> {
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
