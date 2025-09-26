use config::{Deployment, NodeConfig, NodeSource as ConfigNodeSource};

pub use config::GitRemoteSpec;

#[derive(Debug, Clone)]
pub struct ResolvedNodeSource {
    source: Option<ConfigNodeSource>,
    node: NodeConfig,
}

impl ResolvedNodeSource {
    pub fn new(source: Option<ConfigNodeSource>, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> Option<&ConfigNodeSource> {
        self.source.as_ref()
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }

    pub fn into_node(self) -> NodeConfig {
        self.node
    }

    pub fn into_parts(self) -> (Option<ConfigNodeSource>, NodeConfig) {
        (self.source, self.node)
    }
}

#[derive(Debug, Clone)]
pub struct DeploymentMap {
    deployment: Deployment,
    node_source: ResolvedNodeSource,
}

impl DeploymentMap {
    pub fn new(deployment: Deployment, node_source: ResolvedNodeSource) -> Self {
        Self {
            deployment,
            node_source,
        }
    }

    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    pub fn node_source(&self) -> &ResolvedNodeSource {
        &self.node_source
    }

    pub fn into_parts(self) -> (Deployment, ResolvedNodeSource) {
        (self.deployment, self.node_source)
    }
}

#[derive(Debug, Clone)]
pub enum RemoteSpec {
    Git(GitRemoteSpec),
    Http(String),
}

impl RemoteSpec {
    pub fn from_node_source(source: Option<&ConfigNodeSource>) -> Option<Self> {
        match source {
            Some(ConfigNodeSource::Git(spec)) => Some(Self::Git(spec.clone())),
            Some(ConfigNodeSource::Http(url)) => Some(Self::Http(url.clone())),
            _ => None,
        }
    }
}
