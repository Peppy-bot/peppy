use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::types::DeploymentMap;
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::AnyType;
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
        for node_config in state_snapshot.into_values().flatten() {
            local_node_configs.push(node_config);
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
            resolver: Box::new(DefaultDeploymentResolver),
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
                    if map.is_resolved() {
                        if let Err(err) = Self::validate_instance_parameters(
                            map.deployment(),
                            map.node_source().node(),
                        ) {
                            let deployment = map.deployment().clone();
                            let key = (deployment.name.clone(), deployment.tag.clone());
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
                            let node = map.node_source().node().clone();
                            let already_present = self.node_stack.iter().any(|existing| {
                                existing.manifest.name == node.manifest.name
                                    && existing.manifest.tag == node.manifest.tag
                            });
                            if !already_present {
                                self.node_stack.push(node);
                            }
                        }
                    }

                    let dependencies = Self::collect_dependencies(&map);

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
                    let unresolved_deployment = Deployment {
                        name: dep_name.clone(),
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

        let mut dependencies: HashSet<(String, String)> = HashSet::new();

        let mut register_dependency = |name: &str, tag: &str| {
            let name = name.trim();
            let tag = tag.trim();
            if name.is_empty() || tag.is_empty() {
                return;
            }

            dependencies.insert((name.to_string(), tag.to_string()));
        };

        if let Some(topics) = subscriptions.topics.as_ref() {
            for topic in topics {
                if let Some(node) = topic.node.as_deref() {
                    register_dependency(node, &topic.tag);
                }
            }
        }

        if let Some(services) = subscriptions.services.as_ref() {
            for service in services {
                register_dependency(&service.node, &service.tag);
            }
        }

        if let Some(actions) = subscriptions.actions.as_ref() {
            for action in actions {
                register_dependency(&action.node, &action.tag);
            }
        }

        dependencies
            .into_iter()
            .map(|key| DependencyRef { key })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deployment::types::ResolvedNodeSource;
    use config::{
        node::{NodeConfig, NodeConfigParser},
        peppy_config::{
            Deployment, DeploymentNodeSource, GitRemoteSpec, HttpRemoteSpec, PeppyConfig,
        },
    };
    use git2::{ObjectType, Repository, Signature};
    use httptest::{Expectation, Server, matchers::request, responders::status_code};
    use std::{
        collections::HashMap,
        fs,
        io::Write,
        path::{Path, PathBuf},
    };
    use tempfile::{TempDir, tempdir};

    struct StaticResolver {
        nodes: HashMap<(String, String), NodeConfig>,
    }

    impl StaticResolver {
        fn new(nodes: Vec<NodeConfig>) -> Self {
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
            _node_stack: &[NodeConfig],
        ) -> Result<DeploymentMap> {
            let node = self
                .nodes
                .get(&(deployment.name.clone(), deployment.tag.clone()))
                .cloned()
                .ok_or_else(|| Error::NodeNotFound(deployment.name.clone()))?;

            Ok(DeploymentMap::new(
                deployment.clone(),
                ResolvedNodeSource::new(deployment.source.clone(), node),
            ))
        }
    }

    fn node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> NodeConfig {
        let content = if deps.is_empty() {
            format!(
                r#"{{
                    schema_version: 1,
                    manifest: {{ name: "{name}", tag: "{tag}" }}
                }}"#,
                name = name,
                tag = tag
            )
        } else {
            let topics = deps
                .iter()
                .map(|(dep_name, dep_tag)| {
                    format!(
                        "{{ node: \"{dep_name}\", name: \"{dep_name}_topic\", tag: \"{dep_tag}\" }}"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            format!(
                r#"{{
                    schema_version: 1,
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

    fn deployment(
        name: &str,
        tag: &str,
        source: Option<DeploymentNodeSource>,
        optional: bool,
    ) -> Deployment {
        Deployment {
            name: name.to_string(),
            source,
            tag: tag.to_string(),
            optional,
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

    fn create_http_bundle(temp_dir: &Path, bundle_name: &str, manifest_content: &str) -> Vec<u8> {
        let manifest_path = temp_dir.join("peppy.json5");
        fs::write(&manifest_path, manifest_content).expect("write manifest");

        let mut tar_data = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_data);
            tar_builder
                .append_path_with_name(&manifest_path, "peppy.json5")
                .expect("append manifest");
            tar_builder.finish().expect("finish tar");
        }

        let bundle_path = temp_dir.join(bundle_name);
        let bundle_file = fs::File::create(&bundle_path).expect("create bundle");
        let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("create zstd encoder");
        encoder
            .write_all(&tar_data)
            .expect("write compressed bundle");
        encoder.finish().expect("finish encoder");

        fs::read(&bundle_path).expect("read bundle")
    }

    fn create_git_repository(manifest_content: &str, tag: &str) -> TempDir {
        let remote_dir = tempdir().expect("remote temp dir");
        let repo = Repository::init(remote_dir.path()).expect("init git repo");

        let file_path = remote_dir.path().join("peppy.json5");
        fs::write(&file_path, manifest_content).expect("write manifest");

        let rel_path = file_path
            .strip_prefix(remote_dir.path())
            .expect("relative manifest path");

        let mut index = repo.index().expect("repository index");
        index.add_path(rel_path).expect("add manifest to index");
        index.write().expect("write index");

        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");
        let commit_id = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "initial commit",
                &tree,
                &[],
            )
            .expect("create commit");

        let commit = repo
            .find_object(commit_id, Some(ObjectType::Commit))
            .expect("find commit object");
        repo.tag(tag, &commit, &signature, "tag", false)
            .expect("create tag");

        remote_dir
    }

    fn push_git_commit(repo_path: &Path, files: &[(&str, &str)], message: &str) -> git2::Oid {
        let repo = Repository::open(repo_path).expect("open git repo");

        for (relative_path, contents) in files {
            let full_path = repo_path.join(relative_path);
            if let Some(parent) = Path::new(relative_path).parent() {
                fs::create_dir_all(repo_path.join(parent)).expect("create directories for file");
            }
            fs::write(&full_path, contents).expect("write file contents");
        }

        let mut index = repo.index().expect("repo index");
        for (relative_path, _) in files {
            index
                .add_path(Path::new(relative_path))
                .expect("add file to index");
        }
        index.write().expect("write index");

        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");

        let parents: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .expect("create commit")
    }

    #[test]
    fn uses_provided_node_stack() {
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
    fn http_bundle_is_downloaded_and_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let server = Server::run();

        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" }
        }"#;
        let bundle_bytes =
            create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
                .respond_with(status_code(200).body(bundle_bytes)),
        );

        let url = server.url("/bundles/uvc_camera.tar.zst");
        let http_spec =
            HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Http(http_spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "http deployment should resolve to single node"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(node_map.is_resolved(), "http deployment should be resolved");
        assert_eq!(node_map.deployment().name, "uvc_camera");
        assert_eq!(node_map.node_source().node().manifest.tag, "1.2.3");
        assert_eq!(
            node_map.node_source().node().manifest.name.as_str(),
            "uvc_camera"
        );
    }

    #[test]
    fn http_bundle_is_downloaded_and_name_not_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let server = Server::run();

        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera_wrong", tag: "1.2.3" }
        }"#;
        let bundle_bytes =
            create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
                .respond_with(status_code(200).body(bundle_bytes)),
        );

        let url = server.url("/bundles/uvc_camera.tar.zst");
        let http_spec =
            HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Http(http_spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "http deployment should be tracked even when unresolved"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(
            !node_map.is_resolved(),
            "manifest name mismatch should fail resolution"
        );
        let error = node_map
            .error()
            .expect("unresolved deployment should carry error");
        match error {
            Error::DeploymentNotResolvable(identifier, reason) => {
                assert_eq!(identifier, "uvc_camera:1.2.3");
                assert!(
                    reason.contains("node name"),
                    "unexpected error reason: {reason}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn http_bundle_is_downloaded_and_tag_not_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let server = Server::run();

        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "9.9.9" }
        }"#;
        let bundle_bytes =
            create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
        server.expect(
            Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
                .respond_with(status_code(200).body(bundle_bytes)),
        );

        let url = server.url("/bundles/uvc_camera.tar.zst");
        let http_spec =
            HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Http(http_spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "http deployment should be tracked even when unresolved"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(
            !node_map.is_resolved(),
            "manifest tag mismatch should fail resolution"
        );
        let error = node_map
            .error()
            .expect("unresolved deployment should carry error");
        match error {
            Error::DeploymentNotResolvable(identifier, reason) => {
                assert_eq!(identifier, "uvc_camera:1.2.3");
                assert!(reason.contains("tag"), "unexpected error reason: {reason}");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn git_repo_is_cloned_and_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" }
        }"#;
        let remote = create_git_repository(manifest_content, "1.2.3");

        let spec = GitRemoteSpec {
            repo: remote.path().to_string_lossy().to_string(),
            path: None,
        };

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Git(spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "git deployment should resolve to single node"
        );

        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(node_map.is_resolved(), "git deployment should be resolved");
        assert_eq!(node_map.deployment().name, "uvc_camera");
        assert_eq!(node_map.node_source().node().manifest.tag, "1.2.3");
        assert_eq!(
            node_map.node_source().node().manifest.name.as_str(),
            "uvc_camera"
        );
    }

    #[test]
    fn git_repo_is_cloned_and_name_not_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera_wrong", tag: "1.2.3" }
        }"#;
        let remote = create_git_repository(manifest_content, "1.2.3");

        let spec = GitRemoteSpec {
            repo: remote.path().to_string_lossy().to_string(),
            path: None,
        };

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Git(spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "git deployment should be tracked even when unresolved"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(
            !node_map.is_resolved(),
            "manifest name mismatch should fail resolution"
        );
        let error = node_map
            .error()
            .expect("unresolved deployment should carry error");
        match error {
            Error::DeploymentNotResolvable(identifier, reason) => {
                assert_eq!(identifier, "uvc_camera:1.2.3");
                assert!(
                    reason.contains("node name"),
                    "unexpected error reason: {reason}"
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn git_repo_is_cloned_and_tag_not_resolved() {
        let temp_dir = tempdir().expect("temp dir");
        let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "9.9.9" }
        }"#;
        let remote = create_git_repository(manifest_content, "1.2.3");

        let spec = GitRemoteSpec {
            repo: remote.path().to_string_lossy().to_string(),
            path: None,
        };

        let deployments = vec![deployment(
            "uvc_camera",
            "1.2.3",
            Some(DeploymentNodeSource::Git(spec)),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "git deployment should be tracked even when unresolved"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(
            !node_map.is_resolved(),
            "manifest tag mismatch should fail resolution"
        );
        let error = node_map
            .error()
            .expect("unresolved deployment should carry error");
        match error {
            Error::DeploymentNotResolvable(identifier, reason) => {
                assert_eq!(identifier, "uvc_camera:1.2.3");
                assert!(reason.contains("tag"), "unexpected error reason: {reason}");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn git_repo_is_cloned_and_same_tag_updates_code() {
        let temp_dir = tempdir().expect("temp dir");
        let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v1"] }
        }"#;
        let remote = create_git_repository(manifest_v1, "1.0.0");

        let spec = GitRemoteSpec {
            repo: remote.path().to_string_lossy().to_string(),
            path: None,
        };

        let deployments = vec![deployment(
            "uvc_camera",
            "1.0.0",
            Some(DeploymentNodeSource::Git(spec.clone())),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            1,
            "git deployment should resolve to single node on first fetch"
        );

        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(node_map.is_resolved(), "git deployment should resolve");
        let launch_cmd_v1 = node_map
            .node_source()
            .node()
            .manifest
            .launch_cmd
            .clone()
            .expect("launch command present");
        assert_eq!(launch_cmd_v1, vec!["run_v1".to_string()]);

        // Update the remote repository keeping the same tag but new contents.
        let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v2"] }
        }"#;

        let commit_id = push_git_commit(
            remote.path(),
            &[("peppy.json5", manifest_v2)],
            "update manifest",
        );

        let repo = Repository::open(remote.path()).expect("open remote repo");
        let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");

        let commit = repo
            .find_object(commit_id, Some(ObjectType::Commit))
            .expect("find updated commit");
        repo.tag("1.0.0", &commit, &signature, "tag", true)
            .expect("retag commit");

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder.build_with_nodes(Vec::new()).expect("planner");

        let graph = planner.map_deployments_to_nodes();
        assert_eq!(
            graph.len(),
            1,
            "git deployment should resolve to single node on subsequent fetch"
        );
        let root = graph.root_index();
        let node_map = graph.get(root).expect("root node map");
        assert!(
            node_map.is_resolved(),
            "git deployment should still resolve"
        );
        let launch_cmd_v2 = node_map
            .node_source()
            .node()
            .manifest
            .launch_cmd
            .clone()
            .expect("launch command present after update");

        assert_eq!(launch_cmd_v2, vec!["run_v2".to_string()]);
        assert_ne!(launch_cmd_v1, launch_cmd_v2);
    }

    #[test]
    fn optional_dependency_missing_is_unresolved() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![
            deployment(
                "alpha",
                "1.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
                false,
            ),
            deployment(
                "beta",
                "1.0.0",
                Some(DeploymentNodeSource::Git(GitRemoteSpec {
                    repo: "https://example.com/repo.git".to_string(),
                    path: None,
                })),
                true,
            ),
        ];

        let config = PeppyConfig {
            deployments: Some(deployments.clone()),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);

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
            "missing optional deployment should still surface as unresolved when required",
        );

        let root = graph.root_index();
        let deployment_map = graph.get(root).expect("root node");
        assert_eq!(deployment_map.deployment().name, "alpha");
        assert!(deployment_map.is_resolved());

        let beta_map = graph
            .children(root)
            .into_iter()
            .filter_map(|idx| graph.get(idx))
            .find(|map| map.deployment().name == "beta")
            .expect("beta dependency should be present even when optional");

        assert!(!beta_map.is_resolved());
        let error = beta_map
            .error()
            .expect("beta should carry resolution error");
        assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
    }

    #[test]
    fn optional_dependency_with_wrong_tag_is_unresolved() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![
            deployment(
                "alpha",
                "1.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
                false,
            ),
            deployment(
                "beta",
                "2.0.0", // The deployment exists on disk, but the tag does not
                Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
                true,
            ),
        ];

        let config = PeppyConfig {
            deployments: Some(deployments.clone()),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let alpha_node = node_config("alpha", "1.0.0", &[("beta", "2.0.0")]);
        let beta_node = node_config("beta", "1.0.0", &[]);

        let loader_nodes = vec![alpha_node.clone(), beta_node.clone()];
        let resolver = StaticResolver::new(vec![alpha_node, beta_node]);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(loader_nodes)
            .expect("planner")
            .with_resolver(resolver);

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            2,
            "optional deployment with mismatched tag should surface as unresolved",
        );

        let root = graph.root_index();
        let root_map = graph.get(root).expect("root node");
        assert_eq!(root_map.deployment().name, "alpha");
        assert!(root_map.is_resolved());

        let beta_map = graph
            .children(root)
            .into_iter()
            .filter_map(|idx| graph.get(idx))
            .find(|map| map.deployment().name == "beta")
            .expect("beta dependency should be present even when unresolved");

        assert!(!beta_map.is_resolved());
        let error: &Error = beta_map
            .error()
            .expect("beta should carry resolution error");
        assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
    }

    #[test]
    fn required_optional_dependency_surfaces_error() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![
            deployment(
                "alpha",
                "1.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
                true, // Alpha cannot be optional here since beta depends on it and it itself non-optional
            ),
            deployment(
                "beta",
                "2.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
                false,
            ),
        ];

        let config = PeppyConfig {
            deployments: Some(deployments.clone()),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

        let loader_nodes = vec![beta_node.clone()];
        let resolver = StaticResolver::new(vec![beta_node]);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(loader_nodes)
            .expect("planner")
            .with_resolver(resolver);

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            2,
            "optional dependency should surface as unresolved when required by a non-optional deployment",
        );

        let root = graph.root_index();
        let root_map = graph.get(root).expect("root node");
        assert_eq!(root_map.deployment().name, "beta");
        assert!(root_map.is_resolved());

        let alpha_map = graph
            .children(root)
            .into_iter()
            .filter_map(|idx| graph.get(idx))
            .find(|map| map.deployment().name == "alpha")
            .expect("alpha dependency should be present as unresolved");

        assert!(!alpha_map.is_resolved());
        let error = alpha_map
            .error()
            .expect("alpha should carry resolution error");

        assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
    }

    #[test]
    fn unresolved_deployments_remain_in_graph() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![
            deployment(
                "alpha",
                "1.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
                true, // Alpha cannot be optional here since beta depends on it and is itself non-optional
            ),
            deployment(
                "beta",
                "2.0.0",
                Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
                false,
            ),
            deployment(
                "gamma",
                "3.0.0", // This version does not exist
                Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
                false,
            ),
        ];

        let config = PeppyConfig {
            deployments: Some(deployments.clone()),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

        let loader_nodes = vec![beta_node.clone()];
        let resolver = StaticResolver::new(vec![beta_node]);

        let builder =
            LocalNodeStackBuilder::from_root_config_file(&config_path, None).expect("builder");
        let planner = builder
            .build_with_nodes(loader_nodes)
            .expect("planner")
            .with_resolver(resolver);

        let graph = planner.map_deployments_to_nodes();

        assert_eq!(
            graph.len(),
            3,
            "entire deployment list should be represented"
        );

        let unresolved: Vec<_> = graph
            .indices()
            .into_iter()
            .filter_map(|idx| graph.get(idx))
            .filter(|map| !map.is_resolved())
            .collect();

        let mut unresolved_names: Vec<_> = unresolved
            .iter()
            .map(|map| map.deployment().name.clone())
            .collect();
        unresolved_names.sort();
        assert_eq!(
            unresolved_names.len(),
            2,
            "only two deployments should contain errors"
        );

        assert_eq!(
            unresolved_names,
            vec!["alpha".to_string(), "gamma".to_string()]
        );

        let unresolved_errors: Vec<_> = unresolved
            .iter()
            .map(|map| {
                map.error()
                    .expect("unresolved deployment should carry error")
            })
            .collect();

        assert!(
            unresolved_errors
                .iter()
                .all(|error| matches!(error, Error::DeploymentNotResolvable(_, _))),
            "unexpected unresolved deployment error kind",
        );

        let beta_map = graph
            .indices()
            .into_iter()
            .filter_map(|idx| graph.get(idx))
            .find(|map| map.deployment().name == "beta")
            .expect("beta deployment should be present");

        assert!(beta_map.is_resolved());
    }

    #[test]
    fn missing_dependency_becomes_unresolved_node() {
        let temp_dir = tempdir().expect("temp dir");

        let deployments = vec![deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            false,
        )];

        let config = PeppyConfig {
            deployments: Some(deployments),
            logging: None,
        };
        let config_path = write_config(temp_dir.path().join("peppy_config.json5"), config);

        let alpha_node = node_config("alpha", "1.0.0", &[("delta", "1.0.0")]);
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
