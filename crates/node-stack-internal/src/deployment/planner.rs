use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::DeploymentMap;
use super::NodeStack;
use super::types::{
    InterfaceRequirement, collect_dependency_specs, exposes_interface, interface_kind_label,
};
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::AnyType;
use config::FSNodeConfigWatcher;
use config::node::NodeConfig;
use config::peppy_config::{
    Deployment, DeploymentNodeSource, Name, PeppyLauncher, PeppyLauncherParser,
};
use petgraph::{
    Direction,
    stable_graph::{NodeIndex, StableDiGraph},
};

pub trait DeploymentSourceResolver: Send + Sync {
    fn resolve(
        &self,
        nodes_cache_dir: &Path,
        deployment: &Deployment,
        node_stack: &NodeStack,
    ) -> Result<DeploymentMap>;
}

#[derive(Default)]
pub struct DefaultDeploymentResolver;

impl DeploymentSourceResolver for DefaultDeploymentResolver {
    fn resolve(
        &self,
        nodes_cache_dir: &Path,
        deployment: &Deployment,
        node_stack: &NodeStack,
    ) -> Result<DeploymentMap> {
        match deployment.source.as_ref() {
            Some(DeploymentNodeSource::Local(_)) => {
                resolve_local_deployment(deployment, node_stack)
            }
            Some(DeploymentNodeSource::Git(spec)) => {
                resolve_remote_git(nodes_cache_dir, deployment, spec.clone())
            }
            Some(DeploymentNodeSource::Http(spec)) => {
                resolve_remote_url(nodes_cache_dir, deployment, spec.clone())
            }
            None => resolve_local_deployment(deployment, node_stack),
        }
    }
}

pub struct LocalNodeStackBuilder {
    nodes_cache_dir: PathBuf,
    root_dir: PathBuf,
    launch_file: PathBuf,
}

pub struct LauncherPlanner {
    peppy_launcher: PeppyLauncher,
    nodes_cache_dir: PathBuf,
    node_stack: NodeStack,
    resolver: Box<dyn DeploymentSourceResolver>,
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
impl LocalNodeStackBuilder {
    /// # Arguments
    /// * `launch_file` - Path to the peppy launch file
    /// * `nodes_cache_dir` - The dir where nodes are cached (the ones that are pulled remotely or pushed with `peppy push`). Provide `None` to default to `.peppy/nodes`
    pub fn from_launch_file(
        launch_file: impl AsRef<Path>,
        nodes_cache_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let launch_file = PathBuf::from(launch_file.as_ref());

        let root_dir_canon = launch_file
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        if !root_dir_canon.exists() {
            return Err(Error::FileNotFound(launch_file.clone()));
        }

        let nodes_cache_dir_canon = match nodes_cache_dir {
            Some(path) => std::fs::canonicalize(path)?,
            None => root_dir_canon.clone().join(".peppy").join("nodes"),
        };

        Ok(Self {
            nodes_cache_dir: nodes_cache_dir_canon,
            root_dir: root_dir_canon,
            launch_file,
        })
    }

    fn load_peppy_launcher(&self) -> Result<PeppyLauncher> {
        let path = &self.launch_file;

        if !path.exists() || !path.is_file() {
            return Err(Error::FileNotFound(path.clone()));
        }

        PeppyLauncherParser::from_path(&self.launch_file).map_err(Error::Config)
    }

    fn load_nodes_from_fs(root_dir: &Path) -> Result<NodeStack> {
        let watcher = FSNodeConfigWatcher::new(root_dir)?;
        let state_snapshot = watcher.subscribe().borrow().clone();

        let mut local_node_configs = Vec::new();
        for node_config in state_snapshot.into_values().flatten() {
            local_node_configs.push(node_config);
        }

        Ok(NodeStack::from_configs(local_node_configs))
    }

    fn finish(self, node_stack: NodeStack) -> Result<LauncherPlanner> {
        let peppy_config = self.load_peppy_launcher()?;

        Ok(LauncherPlanner::new(
            peppy_config,
            self.nodes_cache_dir,
            node_stack,
        ))
    }

    /// Create the initial node stack based on the peppy config and its
    /// children in the same folder using the filesystem-backed loader.
    pub fn build(self) -> Result<LauncherPlanner> {
        let local_node_configs = Self::load_nodes_from_fs(&self.root_dir)?;
        self.finish(local_node_configs)
    }

    /// Same as [`Self::build`] but allows providing the node stack directly.
    pub fn build_with_nodes(self, node_stack: NodeStack) -> Result<LauncherPlanner> {
        self.finish(node_stack)
    }
}

impl LauncherPlanner {
    fn new(
        peppy_launcher: PeppyLauncher,
        nodes_cache_dir: impl AsRef<Path>,
        node_stack: NodeStack,
    ) -> Self {
        Self {
            peppy_launcher,
            nodes_cache_dir: nodes_cache_dir.as_ref().to_owned(),
            node_stack,
            resolver: Box::new(DefaultDeploymentResolver),
        }
    }

    pub fn with_resolver(mut self, resolver: impl DeploymentSourceResolver + 'static) -> Self {
        self.resolver = Box::new(resolver);
        self
    }

    pub fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    pub fn map_deployments_to_nodes(mut self) -> DeploymentGraph {
        let mut nodes = self.collect_deployment_entries();
        Self::validate_dependency_interfaces(&mut nodes);
        Self::build_deployment_graph(nodes)
    }

    fn collect_deployment_entries(&mut self) -> Vec<NodeEntry> {
        let deployments = self.peppy_launcher.deployments.take().unwrap_or_default();

        let mut entries = Vec::new();

        for deployment in deployments {
            match self
                .resolver
                .resolve(&self.nodes_cache_dir, &deployment, &self.node_stack)
            {
                Ok(map) => {
                    if map.is_resolved() {
                        if let Err(err) = Self::validate_instance_parameters(
                            map.deployment(),
                            map.node_source().node(),
                        ) {
                            let deployment = map.deployment().clone();
                            let key = (deployment.name.to_string(), deployment.tag.clone());
                            let map = DeploymentMap::unresolved(deployment, err);
                            entries.push(NodeEntry {
                                key,
                                map,
                                dependencies: Vec::new(),
                            });
                            continue;
                        }

                        if !matches!(
                            map.node_source().source(),
                            Some(DeploymentNodeSource::Local(_))
                        ) {
                            let node = map.node_source().node();
                            if !self
                                .node_stack
                                .contains(node.manifest.name.as_str(), &node.manifest.tag)
                            {
                                self.node_stack.push_config(node.clone());
                            }
                        }
                    }

                    let dependencies = Self::collect_dependencies(&map);

                    let key = (
                        map.deployment().name.to_string(),
                        map.deployment().tag.clone(),
                    );

                    entries.push(NodeEntry {
                        key,
                        map,
                        dependencies,
                    });
                }
                Err(err) => {
                    let reason = err.to_string();
                    let unresolved_error = Error::DeploymentNotResolvable(
                        format!("{}:{}", deployment.name, deployment.tag),
                        reason,
                    );
                    let map = DeploymentMap::unresolved(deployment.clone(), unresolved_error);
                    entries.push(NodeEntry {
                        key: (deployment.name.to_string(), deployment.tag.clone()),
                        map,
                        dependencies: Vec::new(),
                    });
                }
            }
        }

        entries
    }

    fn validate_dependency_interfaces(entries: &mut [NodeEntry]) {
        let mut resolved_nodes: HashMap<(String, String), NodeConfig> = HashMap::new();

        for entry in entries.iter() {
            if entry.map.is_resolved() {
                resolved_nodes.insert(entry.key.clone(), entry.map.node_source().node().clone());
            }
        }

        for entry in entries.iter_mut() {
            if !entry.map.is_resolved() {
                continue;
            }

            if let Some(error) = Self::missing_interface_error(entry, &resolved_nodes) {
                let deployment = entry.map.deployment().clone();
                entry.map = DeploymentMap::unresolved(deployment, error);
            }
        }
    }

    fn missing_interface_error(
        entry: &NodeEntry,
        resolved_nodes: &HashMap<(String, String), NodeConfig>,
    ) -> Option<Error> {
        for dependency in &entry.dependencies {
            let Some(node) = resolved_nodes.get(&dependency.key) else {
                continue;
            };

            for requirement in &dependency.requirements {
                if !exposes_interface(node, requirement) {
                    return Some(Error::MissingInterface {
                        dependant: entry.key.0.clone(),
                        dependant_tag: entry.key.1.clone(),
                        dependency: dependency.key.0.clone(),
                        dependency_tag: dependency.key.1.clone(),
                        interface_kind: interface_kind_label(requirement.kind()).to_string(),
                        interface_name: requirement.name().to_owned(),
                    });
                }
            }
        }

        None
    }

    fn build_deployment_graph(nodes: Vec<NodeEntry>) -> DeploymentGraph {
        let mut graph: StableDiGraph<DeploymentMap, ()> = StableDiGraph::default();
        let mut node_indices: HashMap<(String, String), NodeIndex> = HashMap::new();
        let mut dependencies_to_link: Vec<(NodeIndex, Vec<DependencyRef>)> = Vec::new();
        let mut root: Option<NodeIndex> = None;
        let mut optional_deployments: HashSet<(String, String)> = HashSet::new();
        let mut optional_unresolved: HashMap<(String, String), NodeEntry> = HashMap::new();

        for entry in nodes {
            let key = entry.key.clone();
            let is_optional = entry.map.deployment().optional;

            if is_optional {
                optional_deployments.insert(key.clone());
            }

            if is_optional && !entry.map.is_resolved() {
                optional_unresolved.insert(key, entry);
                continue;
            }

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

        let mut required_optional_keys: HashSet<(String, String)> = HashSet::new();
        for (index, dependencies) in &dependencies_to_link {
            let dependant_optional = graph
                .node_weight(*index)
                .map(|map| map.deployment().optional)
                .unwrap_or(false);

            if dependant_optional {
                continue;
            }

            for dependency in dependencies {
                if optional_deployments.contains(&dependency.key) {
                    required_optional_keys.insert(dependency.key.clone());
                }
            }
        }

        for key in required_optional_keys {
            if let Some(entry) = optional_unresolved.remove(&key) {
                let NodeEntry {
                    key,
                    map,
                    dependencies,
                } = entry;

                let index = graph.add_node(map);
                node_indices.insert(key, index);
                dependencies_to_link.push((index, dependencies));
            }
        }

        let mut inserted_edges: HashSet<(NodeIndex, NodeIndex)> = HashSet::new();

        for (from_index, dependencies) in dependencies_to_link {
            let dependant_optional = graph
                .node_weight(from_index)
                .map(|map| map.deployment().optional)
                .unwrap_or(false);

            for dependency in dependencies {
                if let Some(&to_index) = node_indices.get(&dependency.key) {
                    if from_index != to_index && inserted_edges.insert((from_index, to_index)) {
                        graph.add_edge(from_index, to_index, ());
                    }
                } else if optional_deployments.contains(&dependency.key) {
                    if dependant_optional {
                        continue;
                    }

                    let (dep_name, dep_tag) = dependency.key.clone();
                    let identifier = format!("{}:{}", dep_name, dep_tag);
                    let error = Error::DeploymentNotResolvable(
                        identifier.clone(),
                        "dependency declared but missing from peppy_config".to_string(),
                    );
                    let Ok(name) = Name::new(dep_name.clone()) else {
                        continue;
                    };
                    let unresolved_deployment = Deployment {
                        name,
                        source: None,
                        tag: dep_tag.clone(),
                        optional: true,
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
                } else {
                    let (dep_name, dep_tag) = dependency.key.clone();
                    let identifier = format!("{}:{}", dep_name, dep_tag);
                    let error = Error::DeploymentNotResolvable(
                        identifier.clone(),
                        "dependency declared but missing from peppy_config".to_string(),
                    );
                    let Ok(name) = Name::new(dep_name.clone()) else {
                        continue;
                    };
                    let unresolved_deployment = Deployment {
                        name,
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

        let specs = collect_dependency_specs(map.node_source().node());
        let mut grouped: HashMap<(String, String), HashSet<InterfaceRequirement>> = HashMap::new();

        for spec in specs {
            let key = (spec.node_name, spec.node_tag);
            grouped.entry(key).or_default().insert(spec.interface);
        }

        grouped
            .into_iter()
            .map(|(key, requirements)| DependencyRef {
                key,
                requirements: requirements.into_iter().collect(),
            })
            .collect()
    }

    fn validate_instance_parameters(
        deployment: &Deployment,
        node: &NodeConfig,
    ) -> std::result::Result<(), Error> {
        let expected = Self::parameter_leaf_paths(&node.parameters);
        if expected.is_empty() {
            return Ok(());
        }

        let mut unexpected: BTreeSet<String> = BTreeSet::new();

        for instance in &deployment.instances {
            let actual = Self::parameter_leaf_paths(&instance.parameters);
            for value in actual.difference(&expected) {
                unexpected.insert(value.clone());
            }
        }

        if unexpected.is_empty() {
            Ok(())
        } else {
            Err(Error::WrongInputParameters {
                deployment: format!("{}:{}", deployment.name, deployment.tag),
                expected: expected.into_iter().collect(),
                unexpected: unexpected.into_iter().collect(),
            })
        }
    }

    fn parameter_leaf_paths(
        parameters: &std::collections::BTreeMap<String, AnyType>,
    ) -> BTreeSet<String> {
        let mut acc = BTreeSet::new();
        for (key, value) in parameters {
            Self::collect_parameter_paths(value, key.clone(), &mut acc);
        }
        acc
    }

    fn collect_parameter_paths(value: &AnyType, current: String, acc: &mut BTreeSet<String>) {
        match value {
            AnyType::Object(map) if !map.is_empty() => {
                if Self::is_array_parameter_schema(map) {
                    acc.insert(current);
                    return;
                }

                for (child_key, child_value) in map {
                    let next = format!("{current}.{child_key}");
                    Self::collect_parameter_paths(child_value, next, acc);
                }
            }
            _ => {
                acc.insert(current);
            }
        }
    }

    fn is_array_parameter_schema(map: &std::collections::BTreeMap<String, AnyType>) -> bool {
        matches!(
            map.get("type"),
            Some(AnyType::String(kind)) if kind.eq_ignore_ascii_case("array")
        )
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
    requirements: Vec<InterfaceRequirement>,
}
