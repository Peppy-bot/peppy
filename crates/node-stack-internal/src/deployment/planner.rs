use std::path::{Path, PathBuf};

use super::types::DeploymentMap;
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::FSNodeConfigWatcher;
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentNodeSource, PeppyConfig, PeppyConfigParser};
use petgraph::{
    Direction,
    stable_graph::{NodeIndex, StableDiGraph},
};

/// 1. Open up all the `peppy.json5` starting from the current dir (or specified with `--node-config`) and create a tree of nodes (the node stack) that contains all the local NodeConfig.
/// The field `is_root_node` determines the root node of the tree. There can only be a single `peppy.json5` with `is_root_node` defined, otherwise the program crashes
/// 2. A "Deployment map" is created based on the `peppy.json5` containing the `is_root_node`. Each deployment maps to a node in the "node stack" as a directed graph so shared dependencies and cycles are preserved.

pub struct LocalNodesMapper {
    nodes_cache_dir: PathBuf,
    root_dir: PathBuf,
    peppy_config_file: PathBuf,
}

pub struct DeploymentsMapper {
    peppy_config: PeppyConfig,
    nodes_cache_dir: PathBuf,
    pub node_stack: Vec<NodeConfig>,
}

#[derive(Debug)]
pub struct DeploymentGraph {
    graph: StableDiGraph<DeploymentMap, ()>,
    root: NodeIndex,
}

impl DeploymentGraph {
    fn new(graph: StableDiGraph<DeploymentMap, ()>, root: NodeIndex) -> Self {
        Self { graph, root }
    }

    pub fn len(&self) -> usize {
        self.graph.node_count()
    }

    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    pub fn root_index(&self) -> NodeIndex {
        self.root
    }

    pub fn get(&self, index: NodeIndex) -> Option<&DeploymentMap> {
        self.graph.node_weight(index)
    }

    pub fn children(&self, index: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .neighbors_directed(index, Direction::Outgoing)
            .collect()
    }

    pub fn parents(&self, index: NodeIndex) -> Vec<NodeIndex> {
        self.graph
            .neighbors_directed(index, Direction::Incoming)
            .collect()
    }

    pub fn indices(&self) -> Vec<NodeIndex> {
        self.graph.node_indices().collect()
    }
}

/// Given a deployment list, finds the corresponding nodes required by
impl LocalNodesMapper {
    /// # Arguments
    /// * `peppy_config_file` - Path to the peppy config file
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`). Provide `None` to default to `.peppy/nodes`
    pub fn from_root_config_file(
        peppy_config_file: impl AsRef<Path>,
        nodes_cache_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let peppy_config_file = PathBuf::from(peppy_config_file.as_ref());

        let root_dir_canon = peppy_config_file
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        if !root_dir_canon.exists() {
            return Err(Error::FileNotFound(peppy_config_file.clone()));
        }

        let nodes_cache_dir_canon = match nodes_cache_dir {
            Some(path) => std::fs::canonicalize(path)?,
            None => root_dir_canon.clone().join(".peppy").join("nodes"),
        };

        Ok(Self {
            nodes_cache_dir: nodes_cache_dir_canon,
            root_dir: root_dir_canon,
            peppy_config_file,
        })
    }

    fn get_peppy_config(&self) -> Result<PeppyConfig> {
        let path = &self.peppy_config_file;

        if !path.exists() || !path.is_file() {
            return Err(Error::FileNotFound(path.clone()));
        }

        PeppyConfigParser::from_path(&self.peppy_config_file).map_err(Error::Config)
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

        let peppy_config = self.get_peppy_config()?;
        Ok(DeploymentsMapper::new(
            peppy_config,
            self.nodes_cache_dir,
            local_node_configs,
        ))
    }
}

impl DeploymentsMapper {
    pub fn new(
        peppy_config: PeppyConfig,
        nodes_cache_dir: impl AsRef<Path>,
        node_stack: Vec<NodeConfig>,
    ) -> Self {
        Self {
            peppy_config,
            nodes_cache_dir: nodes_cache_dir.as_ref().to_owned(),
            node_stack,
        }
    }

    pub fn map_deployments_to_nodes(self) -> DeploymentGraph {
        todo!("Finish")
        //DeploymentGraph::new(graph, root_index)
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
            Some(DeploymentNodeSource::Local(_)) => {
                resolve_local_deployment(&deployment, self.node_stack)
            }
            Some(DeploymentNodeSource::Git(spec)) => {
                resolve_remote_git(nodes_cache_dir, &deployment, spec.clone())
            }
            Some(DeploymentNodeSource::Http(url)) => {
                resolve_remote_url(nodes_cache_dir, &deployment, url.clone())
            }
            None => resolve_local_deployment(&deployment, self.node_stack),
        }
    }
}
