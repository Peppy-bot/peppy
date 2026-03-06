use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use super::types::{NodeStack, collect_dependency_specs, validate_dependency_specs};
use super::{
    ResolvedNode, git::resolve_remote_git, local::resolve_local_deployment, url::resolve_remote_url,
};
use crate::error::{Error, Result};
use config::consts::PeppyDirs;
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentSource, PeppyLauncher, PeppyLauncherParser};
use config::{AnyType, FSNodeConfigIndex, TypeMismatch};

#[derive(Debug)]
pub enum PlannedDeployment {
    Resolved {
        deployment: Deployment,
        node: Box<NodeConfig>,
    },
    Unresolved {
        deployment: Deployment,
        error: Error,
    },
}

impl PlannedDeployment {
    pub fn resolved(deployment: Deployment, node: NodeConfig) -> Self {
        Self::Resolved {
            deployment,
            node: Box::new(node),
        }
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
            Self::Resolved { node, .. } => Some(node.as_ref()),
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
        self.deployments.iter().find(|d| {
            d.node()
                .is_some_and(|node| node.manifest.name.as_str() == name)
        })
    }

    pub fn dependency_errors(&self) -> &[Error] {
        &self.dependency_errors
    }

    /// Validates that the plan is ready for execution.
    ///
    /// Checks that all non-optional deployments are resolved and that there are
    /// no dependency errors. Returns `Ok(())` if the plan is valid, or an error
    /// string describing all validation failures.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut errors = Vec::new();

        for deployment in self.deployments.iter().filter(|d| !d.is_resolved()) {
            let deployment_id = deployment
                .node()
                .map(|node| format!("{}:{}", node.manifest.name.as_str(), node.manifest.tag))
                .unwrap_or_else(|| deployment_source_id(&deployment.deployment().source));
            let reason = deployment
                .error()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown error".to_string());
            errors.push(format!("deployment {deployment_id} failed: {reason}"));
        }

        for dependency_error in &self.dependency_errors {
            errors.push(dependency_error.to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }
}

pub struct LaunchPlan {
    node_stack: NodeStack,
    report: PlanReport,
}

impl LaunchPlan {
    // TODO Might not be needed, we might always just want to use `from_config`
    pub fn from_launch_file(
        core_node: NodeConfig,
        launch_file: impl AsRef<Path>,
        peppy_dirs: &PeppyDirs,
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

        let peppy_launcher = load_peppy_launcher(&launch_file)?;

        Self::from_config(core_node, peppy_launcher, &root_dir, peppy_dirs)
    }

    /// Creates a launch plan from an already-parsed launcher configuration.
    ///
    /// This is useful when the launcher config has been received over the wire
    /// (e.g., via RPC) rather than read from a local file.
    pub fn from_config(
        core_node: NodeConfig,
        peppy_launcher: PeppyLauncher,
        nodes_directory: impl AsRef<Path>,
        peppy_dirs: &PeppyDirs,
    ) -> Result<Self> {
        let nodes_directory = nodes_directory.as_ref();

        if !nodes_directory.exists() {
            return Err(Error::FileNotFound(nodes_directory.to_path_buf()));
        }

        let node_stack = load_nodes_from_fs(nodes_directory, core_node)?;

        Ok(build_launch_plan(peppy_launcher, node_stack, peppy_dirs))
    }

    pub fn with_nodes(
        launch_file: impl AsRef<Path>,
        node_stack: NodeStack,
        peppy_dirs: &PeppyDirs,
    ) -> Result<Self> {
        let peppy_launcher = load_peppy_launcher(launch_file.as_ref())?;

        Ok(build_launch_plan(peppy_launcher, node_stack, peppy_dirs))
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

fn load_peppy_launcher(launch_file: &Path) -> Result<PeppyLauncher> {
    if !launch_file.exists() || !launch_file.is_file() {
        return Err(Error::FileNotFound(launch_file.to_path_buf()));
    }

    PeppyLauncherParser::from_path(launch_file).map_err(Error::Config)
}

fn load_nodes_from_fs(root_dir: &Path, core_node: NodeConfig) -> Result<NodeStack> {
    let state_snapshot = FSNodeConfigIndex::new(root_dir)?.into_state();

    let local_nodes: Vec<(PathBuf, NodeConfig)> = state_snapshot
        .into_iter()
        .filter_map(|(path, result)| result.ok().map(|config| (path, config)))
        .collect();

    let stack = NodeStack::new(core_node, None, root_dir);
    let mut pending = topological_sort_local_nodes(local_nodes);

    loop {
        let mut made_progress = false;
        let mut still_pending = Vec::new();

        for (config_path, node_config) in pending {
            let node_root = config_path
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| root_dir.to_path_buf());

            match stack.push_config(node_config.clone(), false, node_root) {
                Ok(_) => {
                    made_progress = true;
                }
                Err(Error::MissingDependency { .. } | Error::MissingInterface { .. }) => {
                    still_pending.push((config_path, node_config));
                }
                Err(e) => return Err(e),
            }
        }

        if still_pending.is_empty() {
            break;
        }

        if !made_progress {
            for (config_path, node_config) in still_pending {
                let node_root = config_path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| root_dir.to_path_buf());
                stack.push_config(node_config, true, node_root)?;
            }
            break;
        }

        pending = still_pending;
    }

    Ok(stack)
}

fn topological_sort_local_nodes(configs: Vec<(PathBuf, NodeConfig)>) -> Vec<(PathBuf, NodeConfig)> {
    if configs.is_empty() {
        return configs;
    }

    // Compute the sorted indices in a nested scope so we can borrow from `configs`
    // without blocking its move when rebuilding the output vector.
    let sorted_indices = {
        let key_to_idx: HashMap<(&str, &str), usize> = configs
            .iter()
            .enumerate()
            .map(|(idx, (_, config))| {
                (
                    (config.manifest.name.as_str(), config.manifest.tag.as_str()),
                    idx,
                )
            })
            .collect();

        let mut in_degree = vec![0usize; configs.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); configs.len()];

        for (idx, (_, config)) in configs.iter().enumerate() {
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

    let mut indexed_configs: Vec<Option<(PathBuf, NodeConfig)>> =
        configs.into_iter().map(Some).collect();
    let mut result = Vec::with_capacity(indexed_configs.len());
    for idx in sorted_indices {
        if let Some(config) = indexed_configs[idx].take() {
            result.push(config);
        }
    }

    result
}

fn resolve_deployment(
    added_nodes_dir: &Path,
    base_dir: &Path,
    deployment: &Deployment,
) -> Result<ResolvedNode> {
    match &deployment.source {
        DeploymentSource::Local(spec) => resolve_local_deployment(base_dir, spec),
        DeploymentSource::Git(spec) => resolve_remote_git(added_nodes_dir, spec),
        DeploymentSource::Url(spec) => resolve_remote_url(added_nodes_dir, spec),
    }
}

fn build_launch_plan(
    peppy_launcher: PeppyLauncher,
    source_stack: NodeStack,
    peppy_dirs: &PeppyDirs,
) -> LaunchPlan {
    let deployments = peppy_launcher.deployments;
    let added_nodes_dir = peppy_dirs.added_nodes_dir();
    let base_dir = source_stack.root().root_path().to_path_buf();

    let core_node_config = source_stack.root().config().clone();
    let stack = NodeStack::new(core_node_config, None, &added_nodes_dir);

    let mut planned = Vec::with_capacity(deployments.len());

    for deployment in deployments {
        if deployment.instances.is_empty() {
            let error = Error::DeploymentNotResolvable(
                deployment_source_id(&deployment.source),
                "deployment must have at least one instance".to_string(),
            );
            planned.push(PlannedDeployment::unresolved(deployment, error));
            continue;
        }

        let resolved = match resolve_deployment(&added_nodes_dir, &base_dir, &deployment) {
            Ok(resolved) => resolved,
            Err(err) => {
                let reason = err.to_string();
                let unresolved_error = Error::DeploymentNotResolvable(
                    deployment_source_id(&deployment.source),
                    reason,
                );
                planned.push(PlannedDeployment::unresolved(deployment, unresolved_error));
                continue;
            }
        };
        let node = resolved.config;
        let root_path = resolved.root_path;
        let deployment_label = format!("{}:{}", node.manifest.name.as_str(), node.manifest.tag);

        if let Err(err) = validate_instance_parameters(&deployment_label, &deployment, &node) {
            planned.push(PlannedDeployment::unresolved(deployment, err));
            continue;
        }

        // First, push the config (without creating instances)
        if let Err(err) = stack.push_config(node.clone(), true, root_path.clone()) {
            planned.push(PlannedDeployment::unresolved(deployment, err));
            continue;
        }

        // Then spawn instances for each instance in the deployment
        let mut add_failed = None;
        for instance in &deployment.instances {
            let Ok(instance_id) = config::node::Name::new(instance.instance_id.as_str()) else {
                add_failed = Some(Error::DeploymentNotResolvable(
                    deployment_label.clone(),
                    format!("invalid instance id `{}`", instance.instance_id),
                ));
                break;
            };

            if let Err(err) = stack.add_instance(
                node.manifest.name.as_str(),
                &node.manifest.tag,
                Some(&instance_id),
                // TODO/FIXME PIDs are supposed to be tracked for every instance started on the same machine
                None, // PIDs not tracked in deployment plans
            ) {
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

    let dependency_errors = validate_stack_dependencies(&stack);

    LaunchPlan {
        node_stack: stack,
        report: PlanReport {
            deployments: planned,
            dependency_errors,
        },
    }
}

fn validate_instance_parameters(
    deployment_label: &str,
    deployment: &Deployment,
    node: &NodeConfig,
) -> std::result::Result<(), Error> {
    let expected = parameter_leaf_paths(&node.parameters);
    if expected.is_empty() {
        return Ok(());
    }

    let mut unexpected: BTreeSet<String> = BTreeSet::new();

    for instance in &deployment.instances {
        for_each_parameter_leaf_path(&instance.arguments, |path| {
            if !expected.contains(path) {
                unexpected.insert(path.to_owned());
            }
        });

        // Validate parameter types
        if let Err(type_mismatch) =
            validate_parameter_types(&instance.arguments, &node.parameters, "")
        {
            return Err(Error::WrongParameterType {
                deployment: deployment_label.to_string(),
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
            deployment: deployment_label.to_string(),
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

fn validate_stack_dependencies(stack: &NodeStack) -> Vec<Error> {
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

    snapshot
        .iter()
        .flat_map(|entity| {
            validate_dependency_specs(
                entity.config(),
                entity.config().manifest.name.as_str(),
                &entity.config().manifest.tag,
                |name, tag| node_index.get(&(name, tag)).map(|c| (*c).clone()),
            )
        })
        .collect()
}

fn deployment_source_id(source: &DeploymentSource) -> String {
    match source {
        DeploymentSource::Local(spec) => format!("local:{}", spec.local.display()),
        DeploymentSource::Git(spec) => format!("git:{}::{}@{}", spec.repo, spec.path, spec.ref_),
        DeploymentSource::Url(spec) => format!("url:{}#sha256:{}", spec.url, spec.sha256),
    }
}
