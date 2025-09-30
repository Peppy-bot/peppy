use std::path::{Path, PathBuf};

use super::types::{DeploymentMap, ResolvedNodeSource};
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
    root_dir: PathBuf,
    root_node_config_file: PathBuf,
}

pub struct DeploymentsMapper {
    nodes_cache_dir: PathBuf,
    pub node_stack: Vec<NodeConfig>,
}

/// Given a deployment list, finds the corresponding nodes required by
impl LocalNodesMapper {
    /// # Arguments
    /// * `root_node_config` - Path to the root node config
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`). Provide `None` to default to `.peppy/nodes`
    pub fn from_root_config_file(
        root_node_config_file: impl AsRef<Path>,
        nodes_cache_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let root_node_config_file = PathBuf::from(root_node_config_file.as_ref());

        let root_dir_canon = root_node_config_file
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        if !root_dir_canon.exists() {
            return Err(Error::FileNotFound(root_node_config_file.clone()));
        }

        let nodes_cache_dir_canon = match nodes_cache_dir {
            Some(path) => std::fs::canonicalize(path)?,
            None => root_dir_canon.clone().join(".peppy").join("nodes"),
        };

        Ok(Self {
            nodes_cache_dir: nodes_cache_dir_canon,
            root_dir: root_dir_canon,
            root_node_config_file: root_node_config_file,
        })
    }

    fn check_valid_root_node(&self, local_node_configs: &[NodeConfig]) -> Result<()> {
        let root_node_config = NodeConfigParser::from_path(&self.root_node_config_file)?;

        let root_node_names: Vec<_> = local_node_configs
            .iter()
            .filter(|node| node.manifest.is_root_node)
            .map(|node| {
                let manifest = &node.manifest;
                format!("{}:{}", manifest.name.as_str(), manifest.tag.as_str())
            })
            .collect();

        if !root_node_config.manifest.is_root_node {
            return Err(Error::NotRootNode(self.root_node_config_file.clone()));
        }

        if root_node_names.len() > 1 {
            return Err(Error::MultipleRootNode(
                self.root_dir.clone(),
                root_node_names.join(", "),
            ));
        }

        if root_node_names.is_empty() {
            return Err(Error::RootNodeNotFound(self.root_dir.clone()));
        }
        Ok(())
    }

    /// 1st step: Create the initial node stack based on the root node and its children in the same folder
    pub fn get_local_node_stack(self) -> Result<DeploymentsMapper> {
        let mut local_node_configs = Vec::new();
        let watcher = FSNodeConfigWatcher::new(&self.root_dir)?;
        let state_snapshot = watcher.subscribe().borrow().clone();

        for entry in state_snapshot.into_values() {
            if let Ok(node_config) = entry {
                local_node_configs.push(node_config);
            }
        }
        self.check_valid_root_node(&local_node_configs)?;

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
        fn populate_tree(
            tree: &mut Tree<DeploymentMap>,
            parent_index: usize,
            node: &NodeConfig,
            resolver: &DeploymentResolver<'_>,
            nodes_cache_dir: &Path,
        ) {
            let Some(deployments) = node.deployments.clone() else {
                return;
            };

            for deployment in deployments {
                let optional = deployment.optional;
                let name = deployment.name.clone();
                let tag = deployment.tag.clone();
                match resolver.resolve_deployment(nodes_cache_dir, deployment) {
                    Ok(map) => {
                        let child_node = map.node_source().node().clone();
                        let child_index = tree.add_child(parent_index, map);
                        populate_tree(tree, child_index, &child_node, resolver, nodes_cache_dir);
                    }
                    Err(_err) if optional => {
                        // Optional deployments may be skipped if they cannot be resolved.
                    }
                    Err(err) => {
                        panic!(
                            "Failed to resolve deployment {name}:{tag}: {err}",
                            name = name,
                            tag = tag,
                            err = err
                        );
                    }
                }
            }
        }

        let DeploymentsMapper {
            nodes_cache_dir,
            node_stack,
        } = self;

        let root_node = node_stack
            .iter()
            .find(|node| node.manifest.is_root_node)
            .cloned()
            .expect("root node must exist in node stack");

        let mut tree = Tree::new();
        let root_deployment = Deployment {
            name: root_node.manifest.name.as_str().to_owned(),
            source: None,
            tag: root_node.manifest.tag.clone(),
            optional: false,
            instances: Vec::new(),
        };

        let root_map = DeploymentMap::new(
            root_deployment,
            ResolvedNodeSource::new(None, root_node.clone()),
        );

        let root_index = tree.add_node(root_map);

        let resolver = DeploymentResolver::new(&node_stack);
        populate_tree(
            &mut tree,
            root_index,
            &root_node,
            &resolver,
            nodes_cache_dir.as_path(),
        );

        tree
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
