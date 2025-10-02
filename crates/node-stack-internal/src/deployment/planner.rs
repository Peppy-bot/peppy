use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::path::{Path, PathBuf};

use super::types::{DeploymentMap, ResolvedNodeSource};
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::{
    Deployment, FSNodeConfigWatcher, NodeConfig, NodeConfigParser, NodeSource as ConfigNodeSource,
};
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
    root_node_config_file: PathBuf,
}

pub struct DeploymentsMapper {
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

    pub fn map_deployments_to_nodes(self) -> DeploymentGraph {
        fn deployment_key(deployment: &Deployment) -> String {
            let mut key = format!("{}:{}", deployment.name, deployment.tag);
            if let Some(source) = &deployment.source {
                let source_repr = match source {
                    ConfigNodeSource::Local(path) => format!("local:{}", path.display()),
                    ConfigNodeSource::Git(spec) => {
                        let path = spec.path.as_deref().unwrap_or_default();
                        if path.is_empty() {
                            format!("git:{}", spec.repo)
                        } else {
                            format!("git:{}::{}", spec.repo, path)
                        }
                    }
                    ConfigNodeSource::Http(url) => format!("http:{}", url),
                };
                key.push('|');
                key.push_str(&source_repr);
            }
            key
        }

        fn ensure_edge(
            graph: &mut StableDiGraph<DeploymentMap, ()>,
            parent: NodeIndex,
            child: NodeIndex,
        ) {
            if graph.find_edge(parent, child).is_none() {
                graph.add_edge(parent, child, ());
            }
        }

        fn collect_dependency_selectors(node: &NodeConfig) -> Vec<(String, String)> {
            let mut selectors: HashSet<(String, String)> = HashSet::new();

            let Some(subscribes_to) = node.interfaces.subscribes_to.as_ref() else {
                return Vec::new();
            };

            let mut push_selector = |name: &str, tag: &str| {
                let node_name = name.trim();
                let tag = tag.trim();
                if node_name.is_empty() || tag.is_empty() {
                    return;
                }
                selectors.insert((node_name.to_owned(), tag.to_owned()));
            };

            if let Some(topics) = subscribes_to.topics.as_ref() {
                for topic in topics {
                    push_selector(&topic.node, &topic.tag);
                }
            }

            if let Some(services) = subscribes_to.services.as_ref() {
                for service in services {
                    push_selector(&service.node, &service.tag);
                }
            }

            if let Some(actions) = subscribes_to.actions.as_ref() {
                for action in actions {
                    push_selector(&action.node, &action.tag);
                }
            }

            selectors.into_iter().collect()
        }

        fn register_node_index(
            graph: &mut StableDiGraph<DeploymentMap, ()>,
            node_index: NodeIndex,
            nodes_by_name_tag: &mut HashMap<(String, String), Vec<NodeIndex>>,
            pending_dependents: &mut HashMap<(String, String), Vec<NodeIndex>>,
        ) {
            let (name, tag) = {
                let map = graph
                    .node_weight(node_index)
                    .expect("node index inserted in graph");
                (
                    map.deployment().name.clone(),
                    map.deployment().tag.clone(),
                )
            };

            let key = (name, tag);
            let entry = nodes_by_name_tag.entry(key.clone()).or_default();
            if !entry.iter().any(|existing| *existing == node_index) {
                entry.push(node_index);
            }

            if let Some(waiting_dependents) = pending_dependents.remove(&key) {
                for dependent in waiting_dependents {
                    if dependent != node_index {
                        ensure_edge(graph, node_index, dependent);
                    }
                }
            }
        }

        fn link_interface_dependencies(
            graph: &mut StableDiGraph<DeploymentMap, ()>,
            dependent_index: NodeIndex,
            nodes_by_name_tag: &mut HashMap<(String, String), Vec<NodeIndex>>,
            pending_dependents: &mut HashMap<(String, String), Vec<NodeIndex>>,
        ) {
            let Some(map) = graph.node_weight(dependent_index) else {
                return;
            };

            if !map.is_resolved() {
                return;
            }

            let selectors = collect_dependency_selectors(map.node_source().node());
            for (node_name, node_tag) in selectors {
                let key = (node_name.clone(), node_tag.clone());
                if let Some(providers) = nodes_by_name_tag.get(&key) {
                    for provider in providers.iter().copied() {
                        if provider != dependent_index {
                            ensure_edge(graph, provider, dependent_index);
                        }
                    }
                } else {
                    let entry = pending_dependents.entry(key).or_default();
                    if !entry.iter().any(|existing| *existing == dependent_index) {
                        entry.push(dependent_index);
                    }
                }
            }
        }

        fn populate_graph(
            graph: &mut StableDiGraph<DeploymentMap, ()>,
            parent_index: NodeIndex,
            root_index: NodeIndex,
            node: &NodeConfig,
            resolver: &DeploymentResolver<'_>,
            nodes_cache_dir: &Path,
            seen: &mut HashMap<String, NodeIndex>,
            nodes_by_name_tag: &mut HashMap<(String, String), Vec<NodeIndex>>,
            pending_dependents: &mut HashMap<(String, String), Vec<NodeIndex>>,
        ) {
            let Some(deployments) = node.deployments.as_ref() else {
                return;
            };

            for deployment in deployments {
                let deployment = deployment.clone();
                let optional = deployment.optional;
                let deployment_id = deployment_key(&deployment);

                match resolver.resolve_deployment(nodes_cache_dir, deployment.clone()) {
                    Ok(map) => {
                        let child_node = map.node_source().node().clone();
                        match seen.entry(deployment_id.clone()) {
                            Entry::Occupied(entry) => {
                                let child_index = *entry.get();
                                if parent_index != root_index {
                                    ensure_edge(graph, parent_index, child_index);
                                }
                                link_interface_dependencies(
                                    graph,
                                    child_index,
                                    nodes_by_name_tag,
                                    pending_dependents,
                                );
                            }
                            Entry::Vacant(entry) => {
                                let child_index = graph.add_node(map);
                                entry.insert(child_index);
                                register_node_index(
                                    graph,
                                    child_index,
                                    nodes_by_name_tag,
                                    pending_dependents,
                                );

                                if parent_index != root_index {
                                    ensure_edge(graph, parent_index, child_index);
                                }

                                link_interface_dependencies(
                                    graph,
                                    child_index,
                                    nodes_by_name_tag,
                                    pending_dependents,
                                );

                                if child_index != parent_index {
                                    populate_graph(
                                        graph,
                                        child_index,
                                        root_index,
                                        &child_node,
                                        resolver,
                                        nodes_cache_dir,
                                        seen,
                                        nodes_by_name_tag,
                                        pending_dependents,
                                    );
                                }
                            }
                        }
                    }
                    Err(_err) if optional => {
                        continue;
                    }
                    Err(err) => {
                        let reason = err.to_string();
                        let unresolved = DeploymentMap::unresolved(
                            deployment.clone(),
                            Error::DeploymentNotResolvable(deployment_id.clone(), reason),
                        );
                        let child_index = match seen.entry(deployment_id.clone()) {
                            Entry::Occupied(entry) => *entry.get(),
                            Entry::Vacant(entry) => {
                                let index = graph.add_node(unresolved);
                                entry.insert(index);
                                register_node_index(
                                    graph,
                                    index,
                                    nodes_by_name_tag,
                                    pending_dependents,
                                );
                                index
                            }
                        };
                        if parent_index != root_index {
                            ensure_edge(graph, parent_index, child_index);
                        }
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

        let mut graph = StableDiGraph::new();
        let root_index = graph.add_node(root_map);

        let resolver = DeploymentResolver::new(&node_stack);
        let mut seen = HashMap::new();
        let mut nodes_by_name_tag: HashMap<(String, String), Vec<NodeIndex>> = HashMap::new();
        let mut pending_dependents: HashMap<(String, String), Vec<NodeIndex>> = HashMap::new();

        register_node_index(
            &mut graph,
            root_index,
            &mut nodes_by_name_tag,
            &mut pending_dependents,
        );

        let root_key = {
            let root_map = graph
                .node_weight(root_index)
                .expect("root deployment exists");
            deployment_key(root_map.deployment())
        };
        seen.insert(root_key, root_index);

        populate_graph(
            &mut graph,
            root_index,
            root_index,
            &root_node,
            &resolver,
            nodes_cache_dir.as_path(),
            &mut seen,
            &mut nodes_by_name_tag,
            &mut pending_dependents,
        );

        DeploymentGraph::new(graph, root_index)
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
