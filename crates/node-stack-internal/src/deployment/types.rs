use crate::error::{Error, Result};
pub use config::GitRemoteSpec;
use config::{Deployment, NodeConfig, NodeSource};

#[derive(Debug, Clone)]
pub struct ResolvedNodeSource {
    source: Option<NodeSource>,
    node: NodeConfig,
}

impl ResolvedNodeSource {
    pub fn new(source: Option<NodeSource>, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> Option<&NodeSource> {
        self.source.as_ref()
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }

    pub fn into_node(self) -> NodeConfig {
        self.node
    }

    pub fn into_parts(self) -> (Option<NodeSource>, NodeConfig) {
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
        match &self.node_source {
            Ok(_) => None,
            Err(err) => Some(err),
        }
    }

    pub fn into_parts(self) -> (Deployment, Result<ResolvedNodeSource>) {
        (self.deployment, self.node_source)
    }
}

#[derive(Debug, Clone)]
pub enum RemoteSpec {
    Git(GitRemoteSpec),
    Http(String),
}

impl RemoteSpec {
    pub fn from_node_source(source: Option<&NodeSource>) -> Option<Self> {
        match source {
            Some(NodeSource::Git(spec)) => Some(Self::Git(spec.clone())),
            Some(NodeSource::Http(url)) => Some(Self::Http(url.clone())),
            _ => None,
        }
    }
}
