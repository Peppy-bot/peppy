use std::path::{Path, PathBuf};

use super::types::DeploymentMap;
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::{
    Deployment, FSNodeConfigWatcher, NodeConfig, NodeConfigParser, NodeSource as ConfigNodeSource,
};
// TODO: Use easy_tree::Tree to create a tree of nodes dependencies for the deployment stack
use easy_tree::Tree;

/// 1. Open up all the `peppy.json5` starting from the current dir (or specified with `--node-config`) and create a tree of nodes (the node stack) that contains all the local NodeConfig.
/// The field `is_root_node` determines the root node of the tree. There can only be a single `peppy.json5` with `is_root_node` defined, otherwise the program crashes
/// 2. A "Deployment map" is created based on the `peppy.json5` containing the `is_root_node`. Each deployment maps to a node in the "node stack" as an `easy_tree::Tree`.

pub struct LocalNodesMapper {
    nodes_cache_dir: PathBuf,
    root_node_config_file: PathBuf,
}

pub struct DeploymentsMapper {
    nodes_cache_dir: PathBuf,
    node_stack: Vec<NodeConfig>,
}

/// Given a deployment list, finds the corresponding nodes required by
impl LocalNodesMapper {
    /// # Arguments
    ///
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`)
    /// * `root_node_config` - Path to the root node config
    pub fn new(nodes_cache_dir: impl AsRef<Path>, root_node_config: impl AsRef<Path>) -> Self {
        Self {
            nodes_cache_dir: PathBuf::from(nodes_cache_dir.as_ref()),
            root_node_config_file: PathBuf::from(root_node_config.as_ref()),
        }
    }

    /// 1st step: Create the initial node stack based on the root node and its children in the same folder
    pub fn get_initial_node_stack(self) -> Result<DeploymentsMapper> {
        let mut local_node_configs = Vec::new();
        let root_dir = self
            .root_node_config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        if !root_dir.exists() {
            return Err(Error::FileNotFound(self.root_node_config_file.clone()));
        }
        let root_dir_canon = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());
        let watcher = FSNodeConfigWatcher::new(&root_dir_canon)?;
        let state_snapshot = watcher.subscribe().borrow().clone();

        for entry in state_snapshot.into_values() {
            if let Ok(node_config) = entry {
                local_node_configs.push(node_config);
            }
        }

        let root_node_config = NodeConfigParser::from_path(&self.root_node_config_file)?;

        if !root_node_config.manifest.is_root_node {
            return Err(Error::NotRootNode(self.root_node_config_file.clone()));
        }

        let root_node_names: Vec<_> = local_node_configs
            .iter()
            .filter(|node| node.manifest.is_root_node)
            .map(|node| {
                let manifest = &node.manifest;
                format!("{}:{}", manifest.name.as_str(), manifest.tag.as_str())
            })
            .collect();

        if root_node_names.len() > 1 {
            return Err(Error::MultipleRootNode(
                root_dir_canon.clone(),
                root_node_names.join(", "),
            ));
        }

        if root_node_names.is_empty() {
            return Err(Error::RootNodeNotFound(root_dir_canon.clone()));
        }

        Ok(DeploymentsMapper::new(
            self.nodes_cache_dir,
            local_node_configs,
        ))
    }
}

impl DeploymentsMapper {
    pub fn new(nodes_cache_dir: impl AsRef<Path>, node_stack: Vec<NodeConfig>) -> Self {
        Self {
            nodes_cache_dir: nodes_cache_dir.as_ref().to_owned(),
            node_stack,
        }
    }

    pub fn map_deployments_to_nodes(self) -> Tree<DeploymentMap> {
        // TODO: Based on self.node_stack do the following:
        // 1. Extract the root node from self.node_stack
        // 2. Extract the `deployments` from the root node as a Vec<Deployment>
        // 3. For each deployment, use the `DeploymentResolver::resolve_deployment`
        // Notes:
        //  - Ensures the resulting object is a Tree starting from the root_node
        //  -
        todo!()
    }
}

// Pulls deployments that are remote (git/url etc...)
pub struct DeploymentResolver<'a> {
    node_stack: &'a [NodeConfig],
}

impl<'a> DeploymentResolver<'a> {
    pub fn new(node_stack: &'a [NodeConfig]) -> Self {
        Self { node_stack }
    }
    /// Given a deployment, pulls it into the nodes_cache_dir if it's a remote node
    /// or return its path if it's local
    pub fn resolve_deployment(
        &self,
        nodes_cache_dir: impl AsRef<Path>,
        deployment: Deployment,
    ) -> Result<DeploymentMap> {
        let nodes_cache_dir = nodes_cache_dir.as_ref();

        match deployment.source.as_ref() {
            Some(ConfigNodeSource::Local(_)) => {
                resolve_local_deployment(&deployment, self.node_stack)
            }
            Some(ConfigNodeSource::Git(spec)) => {
                resolve_remote_git(nodes_cache_dir, &deployment, spec.clone())
            }
            Some(ConfigNodeSource::Http(url)) => {
                resolve_remote_url(nodes_cache_dir, &deployment, url.clone())
            }
            None => resolve_local_deployment(&deployment, self.node_stack),
        }
    }
}

#[cfg(test)]
mod tests {}
