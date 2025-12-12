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

pub struct DeploymentPlanner {
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

impl DeploymentPlanner {
    /// Creates a new planner by loading the launch file and discovering nodes from the filesystem.
    ///
    /// # Arguments
    /// * `master_node` - The root node configuration for the stack
    /// * `launch_file` - Path to the peppy launch file
    /// * `nodes_cache_dir` - The dir where nodes are cached. Provide `None` to default to `.peppy/nodes`
    pub fn from_launch_file(
        master_node: NodeConfig,
        launch_file: impl AsRef<Path>,
        nodes_cache_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let launch_file = PathBuf::from(launch_file.as_ref());

        let root_dir = launch_file
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        if !root_dir.exists() {
            return Err(Error::FileNotFound(launch_file.clone()));
        }

        let nodes_cache_dir = match nodes_cache_dir {
            Some(path) => std::fs::canonicalize(path)?,
            None => root_dir.join(".peppy").join("nodes"),
        };

        let peppy_launcher = Self::load_peppy_launcher(&launch_file)?;
        let node_stack = Self::load_nodes_from_fs(&root_dir, master_node)?;

        Ok(Self {
            peppy_launcher,
            nodes_cache_dir,
            node_stack,
            resolver: Box::new(DefaultDeploymentResolver),
        })
    }

    /// Creates a new planner with a pre-built node stack.
    ///
    /// This is useful for testing or when nodes are provided from a source other than the filesystem.
    pub fn with_nodes(
        launch_file: impl AsRef<Path>,
        nodes_cache_dir: Option<PathBuf>,
        node_stack: NodeStack,
    ) -> Result<Self> {
        let launch_file = PathBuf::from(launch_file.as_ref());

        let root_dir = launch_file
            .canonicalize()?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let nodes_cache_dir = match nodes_cache_dir {
            Some(path) => std::fs::canonicalize(path)?,
            None => root_dir.join(".peppy").join("nodes"),
        };

        let peppy_launcher = Self::load_peppy_launcher(&launch_file)?;

        Ok(Self {
            peppy_launcher,
            nodes_cache_dir,
            node_stack,
            resolver: Box::new(DefaultDeploymentResolver),
        })
    }

    fn load_peppy_launcher(launch_file: &Path) -> Result<PeppyLauncher> {
        if !launch_file.exists() || !launch_file.is_file() {
            return Err(Error::FileNotFound(launch_file.to_path_buf()));
        }

        PeppyLauncherParser::from_path(launch_file).map_err(Error::Config)
    }

    fn load_nodes_from_fs(root_dir: &Path, master_node: NodeConfig) -> Result<NodeStack> {
        let watcher = FSNodeConfigWatcher::new(root_dir)?;
        let state_snapshot = watcher.subscribe().borrow().clone();

        // Collect all local nodes
        let local_nodes: Vec<NodeConfig> = state_snapshot.into_values().flatten().collect();

        // Create the stack with the master node as root
        let stack = NodeStack::new(master_node, None);

        // Topologically sort local nodes and add them iteratively.
        // Nodes with missing dependencies are retried until no progress can be made.
        // Any remaining nodes are inserted leniently so that launch planning can
        // proceed even when some dependencies are remote.
        let mut pending = Self::topological_sort_local_nodes(local_nodes);

        // Keep trying to add nodes until we make no progress
        loop {
            let mut made_progress = false;
            let mut still_pending = Vec::new();

            for node_config in pending {
                match stack.push_config(&node_config, None) {
                    Ok(_) => {
                        made_progress = true;
                    }
                    Err(Error::MissingDependency { .. } | Error::MissingInterface { .. }) => {
                        still_pending.push(node_config);
                    }
                    Err(e) => return Err(e),
                }
            }

            if still_pending.is_empty() {
                break;
            }

            if !made_progress {
                for node_config in still_pending {
                    stack.push_config_allow_missing(&node_config, None)?;
                }
                break;
            }

            pending = still_pending;
        }

        Ok(stack)
    }

    fn topological_sort_local_nodes(configs: Vec<NodeConfig>) -> Vec<NodeConfig> {
        if configs.is_empty() {
            return configs;
        }

        // Build a map of node key -> index
        let key_to_idx: HashMap<(String, String), usize> = configs
            .iter()
            .enumerate()
            .map(|(idx, config)| {
                (
                    (
                        config.manifest.name.as_str().to_owned(),
                        config.manifest.tag.clone(),
                    ),
                    idx,
                )
            })
            .collect();

        // Build in-degree count based on dependencies (only among local nodes)
        let mut in_degree = vec![0usize; configs.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); configs.len()];

        for (idx, config) in configs.iter().enumerate() {
            let specs = collect_dependency_specs(config);
            for spec in specs {
                let dep_key = (spec.node_name, spec.node_tag);
                if let Some(&dep_idx) = key_to_idx.get(&dep_key) {
                    in_degree[idx] += 1;
                    dependents[dep_idx].push(idx);
                }
            }
        }

        // Kahn's algorithm
        let mut queue: Vec<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(idx, _)| idx)
            .collect();

        let mut sorted_indices = Vec::with_capacity(configs.len());

        while let Some(idx) = queue.pop() {
            sorted_indices.push(idx);
            for &dependent_idx in &dependents[idx] {
                in_degree[dependent_idx] -= 1;
                if in_degree[dependent_idx] == 0 {
                    queue.push(dependent_idx);
                }
            }
        }

        // Add remaining (circular deps) at the end
        for (idx, deg) in in_degree.iter().enumerate() {
            if *deg > 0 {
                sorted_indices.push(idx);
            }
        }

        // Rebuild in sorted order
        let mut indexed_configs: Vec<Option<NodeConfig>> = configs.into_iter().map(Some).collect();
        let mut result = Vec::with_capacity(indexed_configs.len());
        for idx in sorted_indices {
            if let Some(config) = indexed_configs[idx].take() {
                result.push(config);
            }
        }

        result
    }

    pub fn with_resolver(mut self, resolver: impl DeploymentSourceResolver + 'static) -> Self {
        self.resolver = Box::new(resolver);
        self
    }

    pub fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    pub fn create_deployment_graph(mut self) -> DeploymentGraph {
        let mut nodes = self.collect_deployment_entries();
        Self::validate_dependency_interfaces(&mut nodes);
        Self::build_deployment_graph(nodes)
    }

    fn collect_deployment_entries(&mut self) -> Vec<NodeEntry> {
        let deployments = self.peppy_launcher.deployments.take().unwrap_or_default();

        // Phase 1: Resolve all deployments and collect entries (without adding to stack yet)
        let mut resolved_entries: Vec<(NodeEntry, Option<NodeConfig>)> = Vec::new();
        let mut unresolved_entries: Vec<NodeEntry> = Vec::new();

        for deployment in deployments {
            if deployment.instances.is_empty() {
                let error = Error::DeploymentNotResolvable(
                    format!("{}:{}", deployment.name, deployment.tag),
                    "deployment must have at least one instance".to_string(),
                );
                let map = DeploymentMap::unresolved(deployment.clone(), error);
                unresolved_entries.push(NodeEntry {
                    key: (deployment.name.to_string(), deployment.tag.clone()),
                    map,
                    dependencies: Vec::new(),
                });
                continue;
            }

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
                            unresolved_entries.push(NodeEntry {
                                key,
                                map,
                                dependencies: Vec::new(),
                            });
                            continue;
                        }

                        let dependencies = Self::collect_dependencies(&map);
                        let key = (
                            map.deployment().name.to_string(),
                            map.deployment().tag.clone(),
                        );

                        // Determine if we need to add this node to the stack
                        let node_to_add = if !matches!(
                            map.node_source().source(),
                            Some(DeploymentNodeSource::Local(_))
                        ) {
                            let node = map.node_source().node();
                            if !self
                                .node_stack
                                .contains(node.manifest.name.as_str(), &node.manifest.tag)
                            {
                                Some(node.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        resolved_entries.push((
                            NodeEntry {
                                key,
                                map,
                                dependencies,
                            },
                            node_to_add,
                        ));
                    } else {
                        // Unresolved but not an error
                        let dependencies = Self::collect_dependencies(&map);
                        let key = (
                            map.deployment().name.to_string(),
                            map.deployment().tag.clone(),
                        );
                        unresolved_entries.push(NodeEntry {
                            key,
                            map,
                            dependencies,
                        });
                    }
                }
                Err(err) => {
                    let reason = err.to_string();
                    let unresolved_error = Error::DeploymentNotResolvable(
                        format!("{}:{}", deployment.name, deployment.tag),
                        reason,
                    );
                    let map = DeploymentMap::unresolved(deployment.clone(), unresolved_error);
                    unresolved_entries.push(NodeEntry {
                        key: (deployment.name.to_string(), deployment.tag.clone()),
                        map,
                        dependencies: Vec::new(),
                    });
                }
            }
        }

        // Phase 2: Topologically sort resolved entries
        let sorted_entries = Self::topological_sort_entries(resolved_entries);

        // Phase 3: Add nodes to stack in sorted order, marking failures as unresolved
        let mut entries = Vec::new();
        for (mut entry, node_to_add) in sorted_entries {
            if let Some(node) = node_to_add {
                if let Err(err) = self.node_stack.push_config(&node, None) {
                    let deployment = entry.map.deployment().clone();
                    entry.map = DeploymentMap::unresolved(deployment, err);
                    entry.dependencies = Vec::new();
                }
            }
            entries.push(entry);
        }

        // Add unresolved entries at the end
        entries.extend(unresolved_entries);

        entries
    }

    fn topological_sort_entries(
        entries: Vec<(NodeEntry, Option<NodeConfig>)>,
    ) -> Vec<(NodeEntry, Option<NodeConfig>)> {
        if entries.is_empty() {
            return entries;
        }

        // Build a map of node key -> index
        let key_to_idx: HashMap<(String, String), usize> = entries
            .iter()
            .enumerate()
            .map(|(idx, (entry, _))| (entry.key.clone(), idx))
            .collect();

        // Build in-degree count based on dependencies
        let mut in_degree = vec![0usize; entries.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];

        for (idx, (entry, _)) in entries.iter().enumerate() {
            for dep in &entry.dependencies {
                if let Some(&dep_idx) = key_to_idx.get(&dep.key) {
                    in_degree[idx] += 1;
                    dependents[dep_idx].push(idx);
                }
            }
        }

        // Kahn's algorithm
        let mut queue: Vec<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(idx, _)| idx)
            .collect();

        let mut sorted_indices = Vec::with_capacity(entries.len());

        while let Some(idx) = queue.pop() {
            sorted_indices.push(idx);
            for &dependent_idx in &dependents[idx] {
                in_degree[dependent_idx] -= 1;
                if in_degree[dependent_idx] == 0 {
                    queue.push(dependent_idx);
                }
            }
        }

        // Add any remaining (circular deps or external deps) at the end
        for (idx, deg) in in_degree.iter().enumerate() {
            if *deg > 0 {
                sorted_indices.push(idx);
            }
        }

        // Rebuild in sorted order
        let mut indexed_entries: Vec<Option<(NodeEntry, Option<NodeConfig>)>> =
            entries.into_iter().map(Some).collect();
        let mut result = Vec::with_capacity(indexed_entries.len());
        for idx in sorted_indices {
            if let Some(entry) = indexed_entries[idx].take() {
                result.push(entry);
            }
        }

        result
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
        let mut missing_optional_links: Vec<(NodeIndex, (String, String))> = Vec::new();

        for (from_index, dependencies) in dependencies_to_link {
            let dependant_optional = graph
                .node_weight(from_index)
                .map(|map| map.deployment().optional)
                .unwrap_or(false);

            for dependency in dependencies {
                if let Some(&to_index) = node_indices.get(&dependency.key) {
                    if !dependant_optional {
                        if let Some(dep_map) = graph.node_weight(to_index) {
                            if dep_map.deployment().optional && !dep_map.is_resolved() {
                                missing_optional_links.push((from_index, dependency.key.clone()));
                            }
                        }
                    }
                    if from_index != to_index && inserted_edges.insert((from_index, to_index)) {
                        graph.add_edge(from_index, to_index, ());
                    }
                } else if optional_deployments.contains(&dependency.key) {
                    if dependant_optional {
                        continue;
                    }

                    missing_optional_links.push((from_index, dependency.key.clone()));

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

        // Mark non-optional dependants unresolved if they rely on an optional deployment
        // that cannot be resolved.
        for (dependant_index, dep_key) in missing_optional_links {
            let dependant_optional = graph
                .node_weight(dependant_index)
                .map(|map| map.deployment().optional)
                .unwrap_or(false);
            if dependant_optional {
                continue;
            }

            if let Some(map) = graph.node_weight_mut(dependant_index) {
                if map.is_resolved() {
                    let (dep_name, dep_tag) = dep_key;
                    let error = Error::MissingDependency {
                        dependant: map.deployment().name.to_string(),
                        dependant_tag: map.deployment().tag.clone(),
                        dependency: dep_name,
                        dependency_tag: dep_tag,
                    };
                    let deployment = map.deployment().clone();
                    *map = DeploymentMap::unresolved(deployment, error);
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
