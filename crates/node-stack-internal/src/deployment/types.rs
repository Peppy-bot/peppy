use config::{Deployment, DeploymentSource, NodeConfig};

#[derive(Debug, Clone)]
pub enum RemoteSpec {
    Git(GitRemoteSpec),
    Http(String),
}

#[derive(Debug, Clone)]
pub struct GitRemoteSpec {
    pub repo: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NodeSource {
    source: DeploymentSource,
    node: NodeConfig,
}

impl NodeSource {
    pub fn new(source: DeploymentSource, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> &DeploymentSource {
        &self.source
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }
}

#[derive(Debug, Clone)]
pub struct DeploymentMap {
    deployment: Deployment,
    node: NodeSource,
}

impl DeploymentMap {
    pub fn new(deployment: Deployment, node: NodeSource) -> Self {
        Self { deployment, node }
    }

    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    pub fn node_source(&self) -> &NodeSource {
        &self.node
    }
}
