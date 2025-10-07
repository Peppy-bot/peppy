use crate::error::{Error, Result};
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentNodeSource};

#[derive(Debug, Clone)]
pub struct ResolvedNodeSource {
    source: Option<DeploymentNodeSource>,
    node: NodeConfig,
}

impl ResolvedNodeSource {
    pub fn new(source: Option<DeploymentNodeSource>, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> Option<&DeploymentNodeSource> {
        self.source.as_ref()
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }

    pub fn into_node(self) -> NodeConfig {
        self.node
    }

    pub fn into_parts(self) -> (Option<DeploymentNodeSource>, NodeConfig) {
        (self.source, self.node)
    }
}

#[derive(Debug)]
pub struct DeploymentMap {
    deployment: Deployment,
    // Contains an error explaining why the deployment could be be resolved or the actual resolved node
    node_source: Result<ResolvedNodeSource>,
}

impl DeploymentMap {
    pub fn new(deployment: Deployment, node_source: ResolvedNodeSource) -> Self {
        Self {
            deployment,
            node_source: Ok(node_source),
        }
    }

    pub fn unresolved(deployment: Deployment, error: Error) -> Self {
        Self {
            deployment,
            node_source: Err(error),
        }
    }

    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    pub fn node_source(&self) -> &ResolvedNodeSource {
        self.node_source
            .as_ref()
            .expect("deployment is unresolved; call error() to inspect failure")
    }

    pub fn is_resolved(&self) -> bool {
        self.node_source.is_ok()
    }

    pub fn error(&self) -> Option<&Error> {
        self.node_source.as_ref().err()
    }
}
