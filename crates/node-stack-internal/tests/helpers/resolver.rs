use config::{node::NodeConfig, peppy_config::Deployment};
use node_stack::NodeStackError as Error;
use node_stack::{DeploymentMap, DeploymentSourceResolver, NodeStack, ResolvedNodeSource};
use std::{collections::HashMap, path::Path};

type Result<T> = std::result::Result<T, Error>;

pub struct StaticResolver {
    nodes: HashMap<(String, String), NodeConfig>,
}

impl StaticResolver {
    pub fn new(nodes: Vec<NodeConfig>) -> Self {
        let mut map = HashMap::new();
        for node in nodes {
            let key = (
                node.manifest.name.as_str().to_owned(),
                node.manifest.tag.clone(),
            );
            map.insert(key, node);
        }
        Self { nodes: map }
    }
}

impl DeploymentSourceResolver for StaticResolver {
    fn resolve(
        &self,
        _nodes_cache_dir: &Path,
        deployment: &Deployment,
        _node_stack: &NodeStack,
    ) -> Result<DeploymentMap> {
        let node = self
            .nodes
            .get(&(deployment.name.to_string(), deployment.tag.clone()))
            .cloned()
            .ok_or_else(|| Error::NodeNotFound(deployment.name.to_string()))?;

        Ok(DeploymentMap::new(
            deployment.clone(),
            ResolvedNodeSource::new(deployment.source.clone(), node),
        ))
    }
}
