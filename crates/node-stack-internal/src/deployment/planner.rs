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

pub trait DeploymentSourceResolver: Send + Sync {
    fn resolve(
        &self,
        nodes_cache_dir: &Path,
        deployment: &Deployment,
        node_stack: &[NodeConfig],
    ) -> Result<DeploymentMap>;
}

#[derive(Default)]
pub struct DefaultDeploymentResolver;

impl DeploymentSourceResolver for DefaultDeploymentResolver {
    fn resolve(
        &self,
        nodes_cache_dir: &Path,
        deployment: &Deployment,
        node_stack: &[NodeConfig],
    ) -> Result<DeploymentMap> {
        match deployment.source.as_ref() {
            Some(DeploymentNodeSource::Local(_)) => {
                resolve_local_deployment(deployment, node_stack)
            }
            Some(DeploymentNodeSource::Git(spec)) => {
                resolve_remote_git(nodes_cache_dir, deployment, spec.clone())
            }
            Some(DeploymentNodeSource::Http(url)) => {
                resolve_remote_url(nodes_cache_dir, deployment, url.clone())
            }
            None => resolve_local_deployment(deployment, node_stack),
        }
    }
}

pub struct LocalNodeStackBuilder {
    nodes_cache_dir: PathBuf,
    root_dir: PathBuf,
    peppy_config_file: PathBuf,
}

pub struct DeploymentPlanner {
    peppy_config: PeppyConfig,
    nodes_cache_dir: PathBuf,
    node_stack: Vec<NodeConfig>,
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

    fn load_peppy_config(&self) -> Result<PeppyConfig> {
        let path = &self.peppy_config_file;

        if !path.exists() || !path.is_file() {
            return Err(Error::FileNotFound(path.clone()));
        }

        PeppyConfigParser::from_path(&self.peppy_config_file).map_err(Error::Config)
    }

    fn load_nodes_from_fs(root_dir: &Path) -> Result<Vec<NodeConfig>> {
        let watcher = FSNodeConfigWatcher::new(root_dir)?;
        let state_snapshot = watcher.subscribe().borrow().clone();

        let mut local_node_configs = Vec::new();
        for entry in state_snapshot.into_values() {
            if let Ok(node_config) = entry {
                local_node_configs.push(node_config);
            }
        }

        Ok(local_node_configs)
    }

    fn finish(self, node_stack: Vec<NodeConfig>) -> Result<DeploymentPlanner> {
        let peppy_config = self.load_peppy_config()?;

        Ok(DeploymentPlanner::new(
            peppy_config,
            self.nodes_cache_dir,
            node_stack,
        ))
    }

    /// Create the initial node stack based on the peppy config and its
    /// children in the same folder using the filesystem-backed loader.
    pub fn build(self) -> Result<DeploymentPlanner> {
        let local_node_configs = Self::load_nodes_from_fs(&self.root_dir)?;
        self.finish(local_node_configs)
    }

    /// Same as [`Self::build`] but allows providing the node stack directly.
    pub fn build_with_nodes(self, node_stack: Vec<NodeConfig>) -> Result<DeploymentPlanner> {
        self.finish(node_stack)
    }
}

impl DeploymentPlanner {
    fn new(
        peppy_config: PeppyConfig,
        nodes_cache_dir: impl AsRef<Path>,
        node_stack: Vec<NodeConfig>,
    ) -> Self {
        Self {
            peppy_config,
            nodes_cache_dir: nodes_cache_dir.as_ref().to_owned(),
            node_stack,
            resolver: Box::new(DefaultDeploymentResolver::default()),
        }
    }

    pub fn with_resolver(mut self, resolver: impl DeploymentSourceResolver + 'static) -> Self {
        self.resolver = Box::new(resolver);
        self
    }

    pub fn node_stack(&self) -> &[NodeConfig] {
        &self.node_stack
    }

    pub fn map_deployments_to_nodes(mut self) -> DeploymentGraph {
        let nodes = self.collect_deployment_entries();
        Self::build_deployment_graph(nodes)
    }

    fn collect_deployment_entries(&mut self) -> Vec<NodeEntry> {
        let deployments = self.peppy_config.deployments.take().unwrap_or_default();

        let mut entries = Vec::new();

        for deployment in deployments {
            match self
                .resolver
                .resolve(&self.nodes_cache_dir, &deployment, &self.node_stack)
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deployment::types::ResolvedNodeSource;
    use config::{
        node::{NodeConfig, NodeConfigParser},
        peppy_config::{Deployment, DeploymentNodeSource, GitRemoteSpec, PeppyConfig},
    };
    use std::{collections::HashMap, fs, path::PathBuf};
    use tempfile::tempdir;

    struct StaticResolver {
        nodes: HashMap<String, NodeConfig>,
    }

    impl StaticResolver {
        fn new(nodes: Vec<NodeConfig>) -> Self {
            let mut map = HashMap::new();
            for node in nodes {
                let name = node.manifest.name.as_str().to_owned();
                map.insert(name, node);
            }
            Self { nodes: map }
        }
    }

    impl DeploymentSourceResolver for StaticResolver {
        fn resolve(
            &self,
            _nodes_cache_dir: &Path,
            deployment: &Deployment,
            _node_stack: &[NodeConfig],
        ) -> Result<DeploymentMap> {
            let node = self
                .nodes
                .get(&deployment.name)
                .cloned()
                .ok_or_else(|| Error::NodeNotFound(deployment.name.clone()))?;

            Ok(DeploymentMap::new(
                deployment.clone(),
                ResolvedNodeSource::new(deployment.source.clone(), node),
            ))
        }
    }

    fn node_config(name: &str, tag: &str, deps: &[(&str, &str, bool)]) -> NodeConfig {
        let content = if deps.is_empty() {
            format!(
                r#"{{
                    manifest: {{ name: "{name}", tag: "{tag}" }}
                }}"#,
                name = name,
                tag = tag
            )
        } else {
            let topics = deps
                .iter()
                .map(|(dep_name, dep_tag, optional)| {
                    format!(
                        "{{ node: \"{dep_name}\", name: \"{dep_name}_topic\", tag: \"{dep_tag}\", callback: \"on_{dep_name}_topic\", optional: {} }}",
                        optional
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            format!(
                r#"{{
                    manifest: {{ name: "{name}", tag: "{tag}" }},
                    interfaces: {{
                        subscribes_to: {{
                            topics: [ {topics} ]
                        }}
                    }}
                }}"#,
                name = name,
                tag = tag,
                topics = topics
            )
        };

        NodeConfigParser::from_content(&content).expect("parse node config")
    }

    fn deployment(name: &str, tag: &str, source: Option<DeploymentNodeSource>) -> Deployment {
        Deployment {
            name: name.to_string(),
            source,
            tag: tag.to_string(),
            instances: Vec::new(),
        }
    }

    fn minimal_config() -> PeppyConfig {
        PeppyConfig {
            deployments: Some(Vec::new()),
            logging: None,
        }
    }

    fn write_config(path: PathBuf, config: PeppyConfig) -> PathBuf {
        let content = serde_json5::to_string(&config).expect("serialize config");
        fs::create_dir_all(path.parent().expect("dir")).expect("create config directory");
        fs::write(&path, content).expect("write config");
        path
    }

    #[test]
    fn build_with_nodes_uses_injected_nodes() {
        let temp_dir = tempdir().expect("temp dir");
        let config_path =
            write_config(temp_dir.path().join("peppy_config.json5"), minimal_config());

        let expected_nodes = vec![node_config("alpha", "1.0.0", &[])];

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(expected_nodes.clone())
            .expect("planner");

        let stack = planner.node_stack();
        assert_eq!(stack.len(), expected_nodes.len());

        // Vec<(String, String)> = Vec<(node_name, node_tag)>
        let actual_manifests: Vec<(String, String)> = stack
            .iter()
            .map(|node| {
                (
                    node.manifest.name.as_str().to_owned(),
                    node.manifest.tag.clone(),
                )
            })
            .collect();
        let expected_manifests: Vec<(String, String)> = expected_nodes
            .iter()
            .map(|node| {
                (
                    node.manifest.name.as_str().to_owned(),
                    node.manifest.tag.clone(),
                )
            })
            .collect();
        assert_eq!(actual_manifests, expected_manifests);
    }

    #[test]
    fn planner_with_stub_resolver_builds_graph_and_skips_optional_missing_dependencies() {
        todo!("Double check, where do we actually want the `optional` field to be available?");
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![
            deployment(
                "alpha",
                "1.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            ),
            deployment(
                "beta",
                "1.0.0",
                Some(DeploymentNodeSource::Git(GitRemoteSpec {
                    repo: "https://example.com/repo.git".to_string(),
                    path: None,
                })),
            ),
        ];

        let config = PeppyConfig {
            deployments: Some(deployments.clone()),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        // Depends on gamma 2.0.0, which doesn't exist (and is optional)
        let alpha_node = node_config(
            "alpha",
            "1.0.0",
            &[("beta", "1.0.0", false), ("gamma", "2.0.0", true)],
        );
        let beta_node = node_config("beta", "1.0.0", &[]);

        let loader_nodes = vec![alpha_node.clone()];
        let resolver = StaticResolver::new(vec![alpha_node, beta_node]);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(loader_nodes.clone())
            .expect("planner")
            .with_resolver(resolver);

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            2,
            "optional dependency should be ignored when missing"
        );

        let root = graph.root_index();
        let root_map = graph.get(root).expect("root node");
        assert_eq!(root_map.deployment().name, "alpha");
        assert!(root_map.is_resolved());

        let child_names: Vec<_> = graph
            .children(root)
            .into_iter()
            .map(|idx| {
                graph
                    .get(idx)
                    .expect("child node")
                    .deployment()
                    .name
                    .clone()
            })
            .collect();

        assert_eq!(child_names, vec!["beta".to_string()]);
    }

    #[test]
    fn planner_inserts_unresolved_nodes_for_missing_required_dependencies() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let alpha_node = node_config("alpha", "1.0.0", &[("delta", "1.0.0", false)]);
        let loader_nodes = vec![alpha_node.clone()];
        let resolver = StaticResolver::new(vec![alpha_node]);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(loader_nodes.clone())
            .expect("planner")
            .with_resolver(resolver);

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            2,
            "missing dependency should be inserted as unresolved"
        );

        let delta_map = graph
            .indices()
            .into_iter()
            .filter_map(|index| graph.get(index))
            .find(|map| map.deployment().name == "delta")
            .expect("graph should contain unresolved delta dependency");

        assert!(!delta_map.is_resolved());
        let error = delta_map.error().expect("delta should carry error");
        let message = error.to_string();
        assert!(
            message.contains("dependency declared but missing"),
            "unexpected error message: {message}"
        );
    }
}
