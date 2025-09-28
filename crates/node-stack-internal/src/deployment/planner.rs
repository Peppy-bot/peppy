use std::path::{Path, PathBuf};

use super::{local::resolve_local_deployment, remote::resolve_remote_deployment};
use crate::error::{Error, Result};
use config::{
    Deployment, FSNodeConfigWatcher, NodeConfig, NodeConfigParser, NodeSource as ConfigNodeSource,
};
// TODO: Use easy_tree::Tree to create a tree of nodes dependencies for the deployment stack
use easy_tree::Tree;

/// 1. Open up all the `peppy.json5` starting from the current dir (or specified with `--node-config`)
/// 2. Create a tree of nodes (the node stack) that maps all the dependencies of the local nodes between each other.
/// The field `is_root_node` determines the root node of the tree. There can only be a single `peppy.json5` with `is_root_node` defined, otherwise the program crashes
/// 3. A "Deployment map" is created based on the `peppy.json5` containing the `is_root_node`. Each deployment maps to a node in the "node stack"

pub struct LocalNodesMapper {
    nodes_cache_dir: PathBuf,
    root_node_config_file: PathBuf,
}

// Pulls deployments that are remote (git/url etc...)
pub struct DeploymentsResolver {
    deployments: Vec<Deployment>,
    resolved_nodes: Vec<NodeConfig>,
}

pub struct DeploymentsMapper {
    deployments: Vec<Deployment>,
    local_node_configs: Vec<NodeConfig>,
}

struct DeploymentStack {
    stack: Vec<(Deployment, Result<NodeConfig>)>,
}

/// Given a deployment list, finds the corresponding nodes required by
impl LocalNodesMapper {
    /// # Arguments
    ///
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`)
    /// * `deployments` - The deployment list
    /// * `nodes` - The list of all known nodes in the current instance
    pub fn new(nodes_cache_dir: impl AsRef<Path>, root_node_config: impl AsRef<Path>) -> Self {
        Self {
            nodes_cache_dir: PathBuf::from(nodes_cache_dir.as_ref()),
            root_node_config_file: PathBuf::from(root_node_config.as_ref()),
        }
    }

    /// 1st step: Create the initial node stack based on the root node and its children in the same folder
    pub fn get_initial_node_stack(self) -> Result<DeploymentsMapper> {
        let root_node_config = NodeConfigParser::from_path(&self.root_node_config_file)?;

        if !root_node_config.manifest.is_root_node {
            return Err(Error::NotRootNode(self.root_node_config_file.clone()));
        }

        let deployments = root_node_config.deployments.clone().unwrap_or_default();

        let root_dir = self
            .root_node_config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let root_config_path = self
            .root_node_config_file
            .canonicalize()
            .unwrap_or_else(|_| self.root_node_config_file.clone());

        // Add the root node config first
        let mut local_node_configs = vec![root_node_config];

        if root_dir.exists() {
            let root_dir_canon = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());

            let watcher = FSNodeConfigWatcher::new(&root_dir_canon)?;
            let state_snapshot = watcher.subscribe().borrow().clone();

            for (path, entry) in state_snapshot {
                if path == root_config_path || path == self.root_node_config_file {
                    continue;
                }

                if let Ok(node_config) = entry {
                    local_node_configs.push(node_config);
                }
            }
        }

        let deployment_resolver = DeploymentsResolver::pull_deployments(
            &self.nodes_cache_dir,
            deployments,
            &local_node_configs,
        )?;

        Ok(DeploymentsMapper::new(
            deployment_resolver,
            local_node_configs,
        ))
    }
}

impl DeploymentsResolver {
    fn new(deployments: Vec<Deployment>, resolved_nodes: Vec<NodeConfig>) -> Self {
        Self {
            deployments,
            resolved_nodes,
        }
    }

    pub fn pull_deployments(
        nodes_cache_dir: impl AsRef<Path>,
        deployments: Vec<Deployment>,
        nodes: &[NodeConfig],
    ) -> Result<Self> {
        let mut resolved_nodes = Vec::new();
        let nodes_cache_dir = nodes_cache_dir.as_ref();

        for deployment in &deployments {
            match deployment.source.as_ref() {
                Some(ConfigNodeSource::Local(_)) => {
                    resolve_local_deployment(deployment, nodes)?;
                }
                Some(ConfigNodeSource::Git(_)) | Some(ConfigNodeSource::Http(_)) => {
                    let map = resolve_remote_deployment(nodes_cache_dir, deployment)?;
                    let (_, node_source) = map.into_parts();
                    resolved_nodes.push(node_source.into_node());
                }
                None => {
                    resolve_local_deployment(deployment, nodes)?;
                }
            }
        }

        Ok(Self::new(deployments, resolved_nodes))
    }
}

impl DeploymentsMapper {
    pub fn new(
        deployments_resolver: DeploymentsResolver,
        local_node_configs: Vec<NodeConfig>,
    ) -> Self {
        let DeploymentsResolver {
            deployments,
            mut resolved_nodes,
        } = deployments_resolver;

        let mut local_node_configs = local_node_configs;
        local_node_configs.append(&mut resolved_nodes);

        Self {
            deployments,
            local_node_configs,
        }
    }

    // 2nd step: Resolve deployments based on the node configs
    pub fn map_deployments_to_nodes(self) -> DeploymentStack {
        let DeploymentsMapper {
            deployments,
            local_node_configs,
        } = self;

        let stack = deployments
            .into_iter()
            .map(|deployment| {
                let deployment_name = deployment.name.clone();
                let deployment_tag = deployment.tag.clone();

                let resolution = local_node_configs
                    .iter()
                    .find(|node| {
                        let manifest = &node.manifest;
                        manifest.name.as_str() == deployment_name.as_str()
                            && manifest.tag == deployment_tag
                    })
                    .cloned()
                    .ok_or_else(|| Error::NodeNotFound(deployment_name.clone()));

                (deployment, resolution)
            })
            .collect();

        DeploymentStack::new(stack)
    }
}

// Previously called "nodes stack"
impl DeploymentStack {
    pub fn new(stack: Vec<(Deployment, Result<NodeConfig>)>) -> Self {
        Self { stack }
    }
}

#[cfg(test)]
mod tests {}
