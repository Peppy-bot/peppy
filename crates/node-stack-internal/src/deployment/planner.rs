use std::collections::{HashMap, HashSet};
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

    /// 1st step: Create the initial node stack based on the peppy config and its children in the same folder
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

    pub fn map_deployments_to_nodes(mut self) -> DeploymentGraph {
        let nodes = self.collect_deployment_entries();
        Self::build_deployment_graph(nodes)
    }

    fn collect_deployment_entries(&mut self) -> Vec<NodeEntry> {
        let deployments = self.peppy_config.deployments.take().unwrap_or_default();

        let mut entries = Vec::new();

        for deployment in deployments {
            let resolver = DeploymentResolver::new(&self.node_stack);
            match resolver.resolve_deployment(&self.nodes_cache_dir, deployment.clone()) {
                Ok(map) => {
                    let dependencies = Self::collect_dependencies(&map);

                    if map.is_resolved()
                        && !matches!(
                            map.node_source().source(),
                            Some(DeploymentNodeSource::Local(_))
                        )
                    {
                        let node = map.node_source().node().clone();
                        let already_present = self.node_stack.iter().any(|existing| {
                            existing.manifest.name == node.manifest.name
                                && existing.manifest.tag == node.manifest.tag
                        });
                        if !already_present {
                            self.node_stack.push(node);
                        }
                    }

                    let key = (map.deployment().name.clone(), map.deployment().tag.clone());

                    entries.push(NodeEntry {
                        key,
                        map,
                        dependencies,
                    });
                }
                Err(err) => {
                    if deployment.optional {
                        continue;
                    }

                    let reason = err.to_string();
                    let unresolved_error = Error::DeploymentNotResolvable(
                        format!("{}:{}", deployment.name, deployment.tag),
                        reason,
                    );
                    let map = DeploymentMap::unresolved(deployment.clone(), unresolved_error);
                    entries.push(NodeEntry {
                        key: (deployment.name.clone(), deployment.tag.clone()),
                        map,
                        dependencies: Vec::new(),
                    });
                }
            }
        }

        entries
    }

    fn build_deployment_graph(nodes: Vec<NodeEntry>) -> DeploymentGraph {
        let mut graph: StableDiGraph<DeploymentMap, ()> = StableDiGraph::default();
        let mut node_indices: HashMap<(String, String), NodeIndex> = HashMap::new();
        let mut dependencies_to_link: Vec<(NodeIndex, Vec<DependencyRef>)> = Vec::new();
        let mut root: Option<NodeIndex> = None;

        for entry in nodes {
            let NodeEntry {
                key,
                map,
                dependencies,
            } = entry;

            let index = graph.add_node(map);
            if root.is_none() {
                root = Some(index);
            }

            node_indices.insert(key, index);
            dependencies_to_link.push((index, dependencies));
        }

        let mut inserted_edges: HashSet<(NodeIndex, NodeIndex)> = HashSet::new();

        for (from_index, dependencies) in dependencies_to_link {
            for dependency in dependencies {
                if let Some(&to_index) = node_indices.get(&dependency.key) {
                    if from_index != to_index && inserted_edges.insert((from_index, to_index)) {
                        graph.add_edge(from_index, to_index, ());
                    }
                } else if !dependency.optional {
                    let (dep_name, dep_tag) = dependency.key.clone();
                    let identifier = format!("{}:{}", dep_name, dep_tag);
                    let error = Error::DeploymentNotResolvable(
                        identifier.clone(),
                        "dependency declared but missing from peppy_config".to_string(),
                    );
                    let unresolved_deployment = Deployment {
                        name: dep_name.clone(),
                        source: None,
                        tag: dep_tag.clone(),
                        optional: false,
                        instances: Vec::new(),
                    };

                    let map = DeploymentMap::unresolved(unresolved_deployment, error);
                    let missing_index = *node_indices
                        .entry((dep_name, dep_tag))
                        .or_insert_with(|| graph.add_node(map));

                    if from_index != missing_index
                        && inserted_edges.insert((from_index, missing_index))
                    {
                        graph.add_edge(from_index, missing_index, ());
                    }
                }
            }
        }

        let root_index = root.unwrap_or_else(|| NodeIndex::new(0));

        DeploymentGraph::new(graph, root_index)
    }

    fn collect_dependencies(map: &DeploymentMap) -> Vec<DependencyRef> {
        if !map.is_resolved() {
            return Vec::new();
        }

        let node = map.node_source().node();
        let Some(subscriptions) = node.interfaces.subscribes_to.as_ref() else {
            return Vec::new();
        };

        let mut dependencies: HashMap<(String, String), bool> = HashMap::new();

        let mut register_dependency = |name: &str, tag: &str, optional: Option<bool>| {
            let name = name.trim();
            let tag = tag.trim();
            if name.is_empty() || tag.is_empty() {
                return;
            }

            let key = (name.to_string(), tag.to_string());
            let is_optional = optional.unwrap_or(false);
            dependencies
                .entry(key)
                .and_modify(|existing| {
                    if !is_optional {
                        *existing = false;
                    }
                })
                .or_insert(is_optional);
        };

        if let Some(topics) = subscriptions.topics.as_ref() {
            for topic in topics {
                register_dependency(&topic.node, &topic.tag, topic.optional);
            }
        }

        if let Some(services) = subscriptions.services.as_ref() {
            for service in services {
                register_dependency(&service.node, &service.tag, service.optional);
            }
        }

        if let Some(actions) = subscriptions.actions.as_ref() {
            for action in actions {
                register_dependency(&action.node, &action.tag, action.optional);
            }
        }

        dependencies
            .into_iter()
            .map(|(key, optional)| DependencyRef { key, optional })
            .collect()
    }
}

struct NodeEntry {
    key: (String, String),
    map: DeploymentMap,
    dependencies: Vec<DependencyRef>,
}

#[derive(Clone)]
struct DependencyRef {
    key: (String, String),
    optional: bool,
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
