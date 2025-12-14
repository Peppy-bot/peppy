use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use super::types::{NodeStack, collect_dependency_specs, exposes_interface};
use super::{git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url};
use crate::error::{Error, Result};
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentNodeSource, PeppyLauncher, PeppyLauncherParser};
use config::{AnyType, FSNodeConfigWatcher, TypeMismatch};

#[derive(Debug)]
pub enum PlannedDeployment {
    Resolved {
        deployment: Deployment,
        node: NodeConfig,
    },
    Unresolved {
        deployment: Deployment,
        error: Error,
    },
}

impl PlannedDeployment {
    pub fn resolved(deployment: Deployment, node: NodeConfig) -> Self {
        Self::Resolved { deployment, node }
    }

    pub fn unresolved(deployment: Deployment, error: Error) -> Self {
        Self::Unresolved { deployment, error }
    }

    pub fn deployment(&self) -> &Deployment {
        match self {
            Self::Resolved { deployment, .. } | Self::Unresolved { deployment, .. } => deployment,
        }
    }

    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved { .. })
    }

    pub fn node(&self) -> Option<&NodeConfig> {
        match self {
            Self::Resolved { node, .. } => Some(node),
            Self::Unresolved { .. } => None,
        }
    }

    pub fn error(&self) -> Option<&Error> {
        match self {
            Self::Resolved { .. } => None,
            Self::Unresolved { error, .. } => Some(error),
        }
    }
}

#[derive(Debug)]
pub struct PlanReport {
    deployments: Vec<PlannedDeployment>,
    dependency_errors: Vec<Error>,
}

impl PlanReport {
    pub fn deployments(&self) -> &[PlannedDeployment] {
        &self.deployments
    }

    pub fn find_deployment_by_name(&self, name: &str) -> Option<&PlannedDeployment> {
        self.deployments
            .iter()
            .find(|d| d.deployment().name.as_str() == name)
    }

    pub fn dependency_errors(&self) -> &[Error] {
        &self.dependency_errors
    }
}

pub struct LaunchPlan {
    node_stack: NodeStack,
    report: PlanReport,
}

impl LaunchPlan {
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

        let nodes_cache_dir = nodes_cache_dir_for(&root_dir, nodes_cache_dir)?;
        let peppy_launcher = load_peppy_launcher(&launch_file)?;
        let node_stack = load_nodes_from_fs(&root_dir, master_node)?;

        Ok(build_launch_plan(
            peppy_launcher,
            nodes_cache_dir,
            node_stack,
        ))
    }

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

        let nodes_cache_dir = nodes_cache_dir_for(&root_dir, nodes_cache_dir)?;
        let peppy_launcher = load_peppy_launcher(&launch_file)?;

        Ok(build_launch_plan(
            peppy_launcher,
            nodes_cache_dir,
            node_stack,
        ))
    }

    pub fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    pub fn into_node_stack(self) -> NodeStack {
        self.node_stack
    }

    pub fn report(&self) -> &PlanReport {
        &self.report
    }
}

fn nodes_cache_dir_for(root_dir: &Path, nodes_cache_dir: Option<PathBuf>) -> Result<PathBuf> {
    Ok(match nodes_cache_dir {
        Some(path) => std::fs::canonicalize(path)?,
        None => root_dir.join(".peppy").join("nodes"),
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

    let local_nodes: Vec<NodeConfig> = state_snapshot.into_values().flatten().collect();

    let stack = NodeStack::new(master_node, None);
    let mut pending = topological_sort_local_nodes(local_nodes);

    loop {
        let mut made_progress = false;
        let mut still_pending = Vec::new();

        for node_config in pending {
            match stack.push_config(&node_config, None, false) {
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
                stack.push_config(&node_config, None, true)?;
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

    // Compute the sorted indices in a nested scope so we can borrow from `configs`
    // without blocking its move when rebuilding the output vector.
    let sorted_indices = {
        let key_to_idx: HashMap<(&str, &str), usize> = configs
            .iter()
            .enumerate()
            .map(|(idx, config)| {
                (
                    (config.manifest.name.as_str(), config.manifest.tag.as_str()),
                    idx,
                )
            })
            .collect();

        let mut in_degree = vec![0usize; configs.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); configs.len()];

        for (idx, config) in configs.iter().enumerate() {
            for spec in collect_dependency_specs(config) {
                let dep_key = (spec.node_name.as_str(), spec.node_tag.as_str());
                if let Some(&dep_idx) = key_to_idx.get(&dep_key) {
                    in_degree[idx] += 1;
                    dependents[dep_idx].push(idx);
                }
            }
        }

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

        for (idx, deg) in in_degree.iter().enumerate() {
            if *deg > 0 {
                sorted_indices.push(idx);
            }
        }

        sorted_indices
    };

    let mut indexed_configs: Vec<Option<NodeConfig>> = configs.into_iter().map(Some).collect();
    let mut result = Vec::with_capacity(indexed_configs.len());
    for idx in sorted_indices {
        if let Some(config) = indexed_configs[idx].take() {
            result.push(config);
        }
    }

    result
}

fn resolve_deployment(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
    node_stack: &NodeStack,
) -> Result<NodeConfig> {
    match deployment.source.as_ref() {
        Some(DeploymentNodeSource::Local(_)) | None => {
            resolve_local_deployment(deployment, node_stack)
        }
        Some(DeploymentNodeSource::Git(spec)) => {
            resolve_remote_git(nodes_cache_dir, deployment, spec.clone())
        }
        Some(DeploymentNodeSource::Http(spec)) => {
            resolve_remote_url(nodes_cache_dir, deployment, spec.clone())
        }
    }
}

fn build_launch_plan(
    mut peppy_launcher: PeppyLauncher,
    nodes_cache_dir: PathBuf,
    source_stack: NodeStack,
) -> LaunchPlan {
    let deployments = peppy_launcher.deployments.take().unwrap_or_default();

    let master_config = source_stack.root().config().clone();
    let stack = NodeStack::new(master_config, None);

    let mut planned = Vec::with_capacity(deployments.len());
    let mut deployment_optional: HashMap<(String, String), bool> = HashMap::new();

    for deployment in deployments {
        let key = (deployment.name.to_string(), deployment.tag.clone());
        deployment_optional.insert(key.clone(), deployment.optional);

        if deployment.instances.is_empty() {
            let error = Error::DeploymentNotResolvable(
                format!("{}:{}", deployment.name, deployment.tag),
                "deployment must have at least one instance".to_string(),
            );
            planned.push(PlannedDeployment::unresolved(deployment, error));
            continue;
        }

        let node = match resolve_deployment(&nodes_cache_dir, &deployment, &source_stack) {
            Ok(node) => node,
            Err(err) => {
                let reason = err.to_string();
                let unresolved_error = Error::DeploymentNotResolvable(
                    format!("{}:{}", deployment.name, deployment.tag),
                    reason,
                );
                planned.push(PlannedDeployment::unresolved(deployment, unresolved_error));
                continue;
            }
        };

        if let Err(err) = validate_instance_parameters(&deployment, &node) {
            planned.push(PlannedDeployment::unresolved(deployment, err));
            continue;
        }

        let mut add_failed = None;
        for instance in &deployment.instances {
            let Ok(instance_id) = config::node::Name::new(instance.instance_id.as_str()) else {
                add_failed = Some(Error::DeploymentNotResolvable(
                    format!("{}:{}", deployment.name, deployment.tag),
                    format!("invalid instance id `{}`", instance.instance_id),
                ));
                break;
            };

            if let Err(err) = stack.push_config(&node, Some(&instance_id), true) {
                add_failed = Some(err);
                break;
            }
        }

        if let Some(error) = add_failed {
            planned.push(PlannedDeployment::unresolved(deployment, error));
            continue;
        }

        planned.push(PlannedDeployment::resolved(deployment, node));
    }

    let dependency_errors = validate_stack_dependencies(&stack, &deployment_optional);

    LaunchPlan {
        node_stack: stack,
        report: PlanReport {
            deployments: planned,
            dependency_errors,
        },
    }
}

fn validate_instance_parameters(
    deployment: &Deployment,
    node: &NodeConfig,
) -> std::result::Result<(), Error> {
    let expected = parameter_leaf_paths(&node.parameters);
    if expected.is_empty() {
        return Ok(());
    }

    let mut unexpected: BTreeSet<String> = BTreeSet::new();

    for instance in &deployment.instances {
        for_each_parameter_leaf_path(&instance.parameters, |path| {
            if !expected.contains(path) {
                unexpected.insert(path.to_owned());
            }
        });

        // Validate parameter types
        if let Err(type_mismatch) =
            validate_parameter_types(&instance.parameters, &node.parameters, "")
        {
            return Err(Error::WrongParameterType {
                deployment: format!("{}:{}", deployment.name, deployment.tag),
                path: type_mismatch.path,
                expected: type_mismatch.expected,
                actual: type_mismatch.actual,
            });
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
    for_each_parameter_leaf_path(parameters, |path| {
        acc.insert(path.to_owned());
    });
    acc
}

fn for_each_parameter_leaf_path(
    parameters: &std::collections::BTreeMap<String, AnyType>,
    mut visit: impl FnMut(&str),
) {
    let mut path = String::new();
    for (key, value) in parameters {
        path.clear();
        path.push_str(key);
        visit_parameter_leaf_paths(value, &mut path, &mut visit);
    }
}

fn visit_parameter_leaf_paths(value: &AnyType, path: &mut String, visit: &mut dyn FnMut(&str)) {
    match value {
        AnyType::Object(map) if !map.is_empty() => {
            if is_array_parameter_schema(map) {
                visit(path.as_str());
                return;
            }

            for (child_key, child_value) in map {
                let original_len = path.len();
                path.push('.');
                path.push_str(child_key);
                visit_parameter_leaf_paths(child_value, path, visit);
                path.truncate(original_len);
            }
        }
        _ => {
            visit(path.as_str());
        }
    }
}

fn is_array_parameter_schema(map: &std::collections::BTreeMap<String, AnyType>) -> bool {
    matches!(
        map.get("type"),
        Some(AnyType::String(kind)) if kind.eq_ignore_ascii_case("array")
    )
}

/// Validates that instance parameter values match the types declared in the node manifest.
/// Recursively walks through nested objects to validate each leaf value.
fn validate_parameter_types(
    instance_params: &std::collections::BTreeMap<String, AnyType>,
    manifest_params: &std::collections::BTreeMap<String, AnyType>,
    prefix: &str,
) -> std::result::Result<(), TypeMismatch> {
    for (key, instance_value) in instance_params {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", prefix, key)
        };

        let Some(manifest_value) = manifest_params.get(key) else {
            // Unknown parameter - handled by WrongInputParameters check
            continue;
        };

        match (instance_value, manifest_value) {
            // Both are objects - recurse into nested structure
            (AnyType::Object(inst_map), AnyType::Object(man_map)) => {
                // Check if manifest defines an array schema
                if is_array_parameter_schema(man_map) {
                    // Instance should provide an array value, not an object
                    return Err(TypeMismatch {
                        path,
                        expected: "array".to_string(),
                        actual: "object".to_string(),
                    });
                }
                validate_parameter_types(inst_map, man_map, &path)?;
            }
            // Manifest declares a type (string like "f32", "bool", etc.)
            (instance_value, AnyType::String(_)) => {
                instance_value.matches_type_spec(manifest_value, &path)?;
            }
            // Manifest declares an object schema but instance provides a non-object
            (instance_value, AnyType::Object(man_map)) => {
                if is_array_parameter_schema(man_map) {
                    // Expect an array
                    if !matches!(instance_value, AnyType::Array(_)) {
                        return Err(TypeMismatch {
                            path,
                            expected: "array".to_string(),
                            actual: instance_value.type_name().to_string(),
                        });
                    }
                    // Validate array items if $items is specified
                    if let (AnyType::Array(items), Some(item_spec)) =
                        (instance_value, man_map.get("items"))
                    {
                        for (i, item) in items.iter().enumerate() {
                            let item_path = format!("{}[{}]", path, i);
                            item.matches_type_spec(item_spec, &item_path)?;
                        }
                    }
                } else {
                    // Manifest expects an object but got something else
                    return Err(TypeMismatch {
                        path,
                        expected: "object".to_string(),
                        actual: instance_value.type_name().to_string(),
                    });
                }
            }
            // Other cases (e.g., manifest has a literal value) - skip validation
            _ => {}
        }
    }
    Ok(())
}

fn validate_stack_dependencies(
    stack: &NodeStack,
    deployment_optional: &HashMap<(String, String), bool>,
) -> Vec<Error> {
    let mut errors = Vec::new();

    let snapshot = stack.snapshot();
    let node_index: HashMap<(&str, &str), &NodeConfig> = snapshot
        .iter()
        .map(|entity| {
            let config = entity.config();
            (
                (config.manifest.name.as_str(), config.manifest.tag.as_str()),
                config,
            )
        })
        .collect();

    let deployment_optional_index: HashMap<(&str, &str), bool> = deployment_optional
        .iter()
        .map(|((name, tag), optional)| ((name.as_str(), tag.as_str()), *optional))
        .collect();

    for entity in &snapshot {
        let dependant_name = entity.config().manifest.name.as_str().to_owned();
        let dependant_tag = entity.config().manifest.tag.clone();
        let dependant_key = (dependant_name.as_str(), dependant_tag.as_str());
        let dependant_optional = deployment_optional_index
            .get(&dependant_key)
            .copied()
            .unwrap_or(false);

        for spec in collect_dependency_specs(entity.config()) {
            let dependency_name = spec.node_name;
            let dependency_tag = spec.node_tag;
            let dependency_key = (dependency_name.as_str(), dependency_tag.as_str());

            let dependency_optional = deployment_optional_index
                .get(&dependency_key)
                .copied()
                .unwrap_or(false);

            let Some(dependency_config) = node_index.get(&dependency_key).copied() else {
                if dependant_optional && dependency_optional {
                    continue;
                }

                errors.push(Error::MissingDependency {
                    dependant: dependant_name.clone(),
                    dependant_tag: dependant_tag.clone(),
                    dependency: dependency_name,
                    dependency_tag,
                });
                continue;
            };

            if !exposes_interface(dependency_config, &spec.interface) {
                errors.push(Error::MissingInterface {
                    dependant: dependant_name.clone(),
                    dependant_tag: dependant_tag.clone(),
                    dependency: dependency_name,
                    dependency_tag,
                    interface_kind: spec.interface.kind().to_string(),
                    interface_name: spec.interface.name().to_owned(),
                });
            }
        }
    }

    errors
}
