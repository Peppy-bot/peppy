use crate::Result;
use crate::names;
use crate::services::node::cache as node_cache;
use crate::services::repo::cache as repo_cache;
use config::consts::PeppyDirs;
use config::node::NodeConfigParser;
use core_node_api::encoding::{NodeSyncRequest, NodeSyncResponse, RepoResolvedEntry};
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceOrigin, InterfaceVariant};
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_sync(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::NODE_SYNC,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                handle_node_sync_request(context, Arc::clone(&node_stack), peppy_dirs.clone())
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Safely removes the `.peppy` directory by atomically renaming it first.
///
/// This avoids TOCTOU (Time Of Check, Time Of Use) race conditions that can occur
/// when using `exists()` followed by `remove_dir_all()`. If multiple processes try
/// to generate for the same node concurrently, the simple pattern can fail with
/// "Directory not empty" errors when one process adds files while another is deleting.
///
/// The atomic rename approach:
/// 1. Renames `.peppy` → `.peppy-old-{pid}-{timestamp}` (atomic operation)
/// 2. Synchronously deletes the renamed directory
/// 3. Lets the generator create a fresh `.peppy` directory
///
/// The deletion is intentionally synchronous: the next pipeline stage
/// (`process_node_add`) copies the source directory recursively and walks
/// `.peppy-old-{pid}-{timestamp}` (which is not in the excluded list), which
/// would race with a concurrent background deletion and surface as intermittent
/// "No such file or directory" errors. Callers already run inside
/// `tokio::task::spawn_blocking`, so the synchronous cost is acceptable.
fn remove_previous_peppy_dir(node_root_dir: &std::path::Path) {
    let peppy_output_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);

    // Path-string sanity check; pure CPU, no syscall.
    if peppy_output_dir.file_name() != Some(std::ffi::OsStr::new(config::consts::PEPPY_OUTPUT_DIR))
    {
        debug!(
            "Unexpected directory name, expected {}: {}",
            config::consts::PEPPY_OUTPUT_DIR,
            peppy_output_dir.display()
        );
        return;
    }

    // One `metadata` call decides "missing", "is a file", or "is a dir".
    // We refuse to rename a non-directory because a stray file at this
    // path would otherwise be silently moved to `.peppy-old-{pid}-{ts}`
    // and stranded there.
    match std::fs::metadata(&peppy_output_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            debug!(
                "Expected .peppy to be a directory, but it's a file: {}",
                peppy_output_dir.display()
            );
            return;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            debug!(
                "Cannot stat .peppy at {}: {}, proceeding with rename anyway",
                peppy_output_dir.display(),
                e
            );
        }
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let old_peppy_dir =
        node_root_dir.join(format!(".peppy-old-{}-{}", std::process::id(), timestamp));

    match std::fs::rename(&peppy_output_dir, &old_peppy_dir) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_dir_all(&old_peppy_dir) {
                // Best-effort: the next stage may copy this stray directory and
                // fail, but that surfaces a real error rather than silently
                // leaving the dir behind.
                debug!(
                    "Failed to remove renamed .peppy directory at {}: {}",
                    old_peppy_dir.display(),
                    e
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Directory was already removed by another process, that's fine
        }
        Err(e) => {
            // Log warning but proceed - generator will create directories as needed.
            // This handles edge cases like permission issues or concurrent renames.
            debug!(
                "Failed to move old .peppy directory: {}, proceeding with generation",
                e
            );
        }
    }
}

/// Returns `true` when the `.peppy` directory under `node_root_dir` is absent
/// or incomplete and must be (re-)generated.
///
/// A complete `.peppy` directory contains:
/// - `git.hash` (non-empty)
/// - `libs/peppygen/peppy.json5.sha256` (non-empty)
fn needs_sync(node_root_dir: &std::path::Path) -> bool {
    let peppy_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if !peppy_dir.exists() {
        return true;
    }

    // git.hash must be a regular non-empty file
    match std::fs::metadata(peppy_dir.join("git.hash")) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {}
        _ => return true,
    }

    // peppygen fingerprint must be a regular non-empty file
    match std::fs::metadata(peppy_dir.join("libs/peppygen/peppy.json5.sha256")) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {}
        _ => return true,
    }

    false
}

async fn handle_node_sync_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_sync_request_inner(&context, &node_stack, peppy_dirs)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_sync_request_inner(
    context: &ServiceRequestContext,
    node_stack: &NodeStack,
    peppy_dirs: PeppyDirs,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeSyncRequest::decode(payload.as_ref())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeSyncResponse::failure("Missing `node_root_dir` in node_sync request")
            .encode()
            .map_err(Into::into);
    }

    if !request.node_root_dir.exists() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode()
        .map_err(Into::into);
    }

    if !request.node_root_dir.is_dir() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode()
        .map_err(Into::into);
    }

    // Optional repository fallback. When `include_repositories` is set, walk
    // the dependency tree, fetch every dep absent from the node stack through
    // the repository cache (FS / git / HTTP), and stash the resolved configs
    // so the resolver closure can use them as a second tier. Stack lookups
    // still win.
    let node_config_path = request.node_root_dir.join(config::consts::NODE_CONFIG_FILE);
    let (repo_resolved, repo_resolved_provenance, bfs_stack_hits) = if request.include_repositories
    {
        if !node_config_path.exists() {
            return NodeSyncResponse::failure(format!(
                "Node config file does not exist: {}",
                node_config_path.display()
            ))
            .encode()
            .map_err(Into::into);
        }
        let parsed = match NodeConfigParser::from_path(&node_config_path) {
            Ok(p) => p,
            Err(e) => {
                return NodeSyncResponse::failure(format!("Failed to parse node config: {}", e))
                    .encode()
                    .map_err(Into::into);
            }
        };
        match materialize_repo_deps(&parsed.manifest, node_stack, &peppy_dirs).await {
            Ok((resolved, provenance, stack_hits)) => (resolved, provenance, Some(stack_hits)),
            Err(reason) => {
                return NodeSyncResponse::failure(reason)
                    .encode()
                    .map_err(Into::into);
            }
        }
    } else {
        (HashMap::new(), Vec::new(), None)
    };

    // Resolver closure: node stack first, then any repository-materialized
    // deps. Stack always wins — the repo cache is a fallback opt-in via
    // the request's `include_repositories` flag.
    let resolve_dep = |name: &str, tag: &str| -> Option<config::node::NodeConfig> {
        node_stack
            .find(name, tag)
            .map(|e| e.read().config().clone())
            .or_else(|| {
                repo_resolved
                    .get(&(name.to_owned(), tag.to_owned()))
                    .cloned()
            })
    };

    // Validate dependencies before generation and collect consumed interfaces
    let (consumed_interfaces, language, root_manifest) = if !node_config_path.exists() {
        return NodeSyncResponse::failure(format!(
            "Node config file does not exist: {}",
            node_config_path.display()
        ))
        .encode()
        .map_err(Into::into);
    } else {
        match NodeConfigParser::from_path(&node_config_path) {
            Ok(node_config) => {
                let dep_errors = node_stack::validate_dependency_specs(
                    &node_config.manifest,
                    &node_config.interfaces,
                    node_config.manifest.name.as_str(),
                    &node_config.manifest.tag,
                    resolve_dep,
                );

                let mut missing_dependencies: HashSet<String> = HashSet::new();
                let mut missing_interfaces: Vec<String> = Vec::new();

                for err in &dep_errors {
                    match err {
                        node_stack::NodeStackError::MissingDependency {
                            dependency,
                            dependency_tag,
                            ..
                        } => {
                            missing_dependencies
                                .insert(format!("{}:{}", dependency, dependency_tag));
                        }
                        node_stack::NodeStackError::MissingInterface {
                            dependency,
                            dependency_tag,
                            interface_kind,
                            interface_name,
                            ..
                        } => {
                            missing_interfaces.push(format!(
                                "expects {} `{}` from `{}:{}`, but it is not exposed",
                                interface_kind.to_lowercase(),
                                interface_name,
                                dependency,
                                dependency_tag
                            ));
                        }
                        node_stack::NodeStackError::UndeclaredLinkId { link_id, .. } => {
                            missing_interfaces
                                .push(format!("references undeclared link_id `{}`", link_id));
                        }
                        _ => {}
                    }
                }

                if !missing_dependencies.is_empty() || !missing_interfaces.is_empty() {
                    let mut errors: Vec<String> = Vec::new();

                    if !missing_dependencies.is_empty() {
                        let mut sorted_deps: Vec<_> = missing_dependencies.iter().collect();
                        sorted_deps.sort();
                        errors.push(format!(
                            "depends on {}, but {} not exist in the stack",
                            sorted_deps
                                .iter()
                                .map(|d| format!("`{}`", d))
                                .collect::<Vec<_>>()
                                .join(", "),
                            if missing_dependencies.len() == 1 {
                                "it does"
                            } else {
                                "they do"
                            }
                        ));
                    }

                    for iface_error in missing_interfaces {
                        errors.push(iface_error);
                    }

                    return NodeSyncResponse::failure(format!(
                        "`{}:{} {}",
                        node_config.manifest.name.as_str(),
                        node_config.manifest.tag,
                        errors.join("; ")
                    ))
                    .encode()
                    .map_err(Into::into);
                }

                // Validate `depends_on.interfaces` entries can be loaded from
                // the local interface cache (SHA pins matched, no on-disk
                // drift). Fail fast with a typed error before the heavier
                // resolver runs.
                if let Err(reason) =
                    validate_interface_dependency_specs(&node_config.manifest, &peppy_dirs)
                {
                    return NodeSyncResponse::failure(format!(
                        "Failed to validate interface dependencies: {reason}"
                    ))
                    .encode()
                    .map_err(Into::into);
                }

                // Collect consumed interfaces with resolved message formats
                let mut interfaces = match collect_consumed_interfaces(
                    &node_config.manifest,
                    &node_config.interfaces,
                    resolve_dep,
                    &peppy_dirs,
                ) {
                    Ok(v) => v,
                    Err(reason) => {
                        return NodeSyncResponse::failure(format!(
                            "Failed to resolve consumed interfaces: {}",
                            reason
                        ))
                        .encode()
                        .map_err(Into::into);
                    }
                };
                let conformed = match resolve_conforms_to(&node_config.interfaces, &peppy_dirs) {
                    Ok(v) => v,
                    Err(reason) => {
                        return NodeSyncResponse::failure(format!(
                            "Failed to resolve `conforms_to` interfaces: {}",
                            reason
                        ))
                        .encode()
                        .map_err(Into::into);
                    }
                };
                interfaces.extend(conformed);
                let language = node_config.execution.language;
                let root_manifest = node_config.manifest.clone();
                (interfaces, language, root_manifest)
            }
            Err(e) => {
                return NodeSyncResponse::failure(format!("Failed to parse node config: {}", e))
                    .encode()
                    .map_err(Into::into);
            }
        }
    };

    let NodeSyncRequest {
        node_root_dir,
        git_hash,
        include_repositories: _,
    } = request;

    // Provenance: record every dep that resolves through the stack so the
    // response can list them under "Synchronized from node stack:" in the
    // CLI's verbose output. When `include_repositories` is on, the BFS in
    // `materialize_repo_deps` already surfaces direct + transitive stack
    // hits (transitive ones reached through repo-cache-materialized
    // parents); we use that result directly. When the flag is off we
    // can't walk repo-cache nodes, so we fall back to a direct-deps pass
    // over the root manifest.
    let resolved_from_stack: Vec<String> = bfs_stack_hits.unwrap_or_else(|| {
        root_manifest
            .depends_on
            .as_ref()
            .map(|d| {
                let mut acc: Vec<String> = Vec::new();
                let mut seen: HashSet<(String, String)> = HashSet::new();
                for dep in &d.nodes {
                    let name = dep.name.as_str().to_owned();
                    let tag = dep.tag.clone();
                    if !seen.insert((name.clone(), tag.clone())) {
                        continue;
                    }
                    if node_stack.find(&name, &tag).is_some() {
                        acc.push(format!("{}:{}", name, tag));
                    }
                }
                acc
            })
            .unwrap_or_default()
    });

    // Generate peppygen for the root node.
    match tokio::task::spawn_blocking(move || -> Result<()> {
        remove_previous_peppy_dir(&node_root_dir);
        generate_peppygen_for_node(
            language,
            &node_root_dir,
            consumed_interfaces,
            &git_hash,
            &peppy_dirs,
            generator::CrateDeployMode::default(),
            None,
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(crate::Error::GeneratorError(e))) => {
            return NodeSyncResponse::failure(format!("Failed to generate peppygen: {}", e))
                .encode()
                .map_err(Into::into);
        }
        Ok(Err(crate::Error::Io(e))) => {
            return NodeSyncResponse::failure(format!("Failed to write git hash file: {}", e))
                .encode()
                .map_err(Into::into);
        }
        Ok(Err(e)) => {
            return NodeSyncResponse::failure(format!("Failed to sync node: {}", e))
                .encode()
                .map_err(Into::into);
        }
        Err(e) => {
            return NodeSyncResponse::failure(format!(
                "Failed to generate peppygen (generate task failed): {}",
                e
            ))
            .encode()
            .map_err(Into::into);
        }
    };

    NodeSyncResponse::success_with_provenance(resolved_from_stack, repo_resolved_provenance)
        .encode()
        .map_err(Into::into)
}

/// Walks `manifest.depends_on.nodes` and BFS-fetches every dependency
/// missing from `node_stack` through the repository cache. Returns
/// `(name, tag) -> NodeConfig` for every materialized dep, a provenance
/// vec for repo-resolved entries, and a `name:tag` list of every dep
/// the BFS found in the node stack (direct or transitive via a
/// repo-cache-materialized parent) so the response can surface them
/// under "Synchronized from node stack:".
///
/// A dep that resolves through `node_stack` is recorded as a stack hit
/// and BFS expansion stops there (the existing resolver tier handles
/// transitive walking via stack entries). A dep that resolves through
/// neither the stack nor any configured repository is a hard failure:
/// the returned `Err` becomes the request's `NodeSyncResponse::failure(...)`
/// payload.
async fn materialize_repo_deps(
    manifest: &config::node::Manifest,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<
    (
        HashMap<(String, String), config::node::NodeConfig>,
        Vec<RepoResolvedEntry>,
        Vec<String>,
    ),
    String,
> {
    let mut resolved: HashMap<(String, String), config::node::NodeConfig> = HashMap::new();
    let mut provenance: Vec<RepoResolvedEntry> = Vec::new();
    let mut stack_hits: Vec<String> = Vec::new();

    let Some(deps) = manifest.depends_on.as_ref() else {
        return Ok((resolved, provenance, stack_hits));
    };
    if deps.nodes.is_empty() {
        return Ok((resolved, provenance, stack_hits));
    }

    // Lazy-load the nodes cache: defer the read until the first stack
    // miss so a manifest fully covered by the NodeStack never touches
    // nodes.json5 (and a malformed cache can't fail a sync that
    // wouldn't have used it). Loaded once for the whole BFS so the
    // `mtime`-keyed memo + checkout dedup amortize across deps.
    let mut cache: Option<(
        Vec<repo_cache::NodeCacheEntry>,
        Option<std::time::SystemTime>,
    )> = None;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut pending: Vec<(String, String)> = deps
        .nodes
        .iter()
        .map(|n| (n.name.as_str().to_owned(), n.tag.clone()))
        .collect();

    while let Some((name, tag)) = pending.pop() {
        if !seen.insert((name.clone(), tag.clone())) {
            continue;
        }
        // Stack tier wins — record the hit (de-duped via `seen` above)
        // and skip materialization for anything already pushed onto the
        // persistent NodeStack.
        if node_stack.find(&name, &tag).is_some() {
            stack_hits.push(format!("{}:{}", name, tag));
            continue;
        }
        if cache.is_none() {
            cache = Some(
                repo_cache::load_with_generation(peppy_dirs)
                    .map_err(|e| format!("failed to load nodes cache: {e}"))?,
            );
        }
        let (entries, cache_generation) = cache.as_ref().expect("cache loaded above");
        let Some(entry) = repo_cache::lookup(entries, &name, &tag) else {
            return Err(format!(
                "dep `{name}:{tag}` not found in node stack or repository cache; \
                 run `peppy repo refresh`"
            ));
        };
        let entry = entry.clone();
        let source_kind = entry.source_type;
        let (_root_dir, parsed) = node_cache::materialize_entry(
            &entry,
            peppy_dirs,
            *cache_generation,
            node_cache::silent_feedback(),
        )
        .await
        .map_err(|e| format!("failed to materialize {name}:{tag} from repo cache: {e}"))?;

        // Push transitive deps onto the BFS queue, skipping anything we
        // already plan to visit. Stack-tier shadowing happens at pop time.
        if let Some(child_deps) = parsed.manifest.depends_on.as_ref() {
            for child in &child_deps.nodes {
                let key = (child.name.as_str().to_owned(), child.tag.clone());
                if !seen.contains(&key) {
                    pending.push(key);
                }
            }
        }

        resolved.insert((name.clone(), tag.clone()), parsed);
        provenance.push(RepoResolvedEntry {
            name,
            tag,
            source_kind,
        });
    }

    Ok((resolved, provenance, stack_hits))
}

/// Discriminator carried by [`DependencyLookupEntry`] so the resolver knows
/// whether a `link_id` resolves to a `depends_on.nodes` entry (load
/// offerings from the producer node config) or a `depends_on.interfaces`
/// entry (load the contract directly from the interface cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DependencyKind {
    Node,
    Interface,
}

/// Resolved `(name, tag, link_id, from_any, kind)` for a single dependency
/// referenced by a consumer's `interfaces.topics.consumes`,
/// `interfaces.services.consumes`, or `interfaces.actions.consumes`.
#[derive(Debug, Clone)]
pub(crate) struct DependencyLookupEntry {
    pub name: String,
    pub tag: String,
    pub sha256: Option<String>,
    pub from_any: bool,
    pub kind: DependencyKind,
}

/// Builds a lookup from `link_id` → [`DependencyLookupEntry`] using the
/// node's `depends_on.nodes` and `depends_on.interfaces`. Parse-time
/// validation has already guaranteed uniqueness of `link_id` across both
/// lists, so insertion-order is preserved.
fn build_dependency_lookup(
    manifest: &config::node::Manifest,
) -> std::collections::HashMap<String, DependencyLookupEntry> {
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return std::collections::HashMap::new();
    };
    let mut out = std::collections::HashMap::new();
    for node in &depends_on.nodes {
        out.insert(
            node.link_id.clone(),
            DependencyLookupEntry {
                name: node.name.as_str().to_string(),
                tag: node.tag.clone(),
                sha256: None,
                from_any: node.from_any,
                kind: DependencyKind::Node,
            },
        );
    }
    for iface in &depends_on.interfaces {
        out.insert(
            iface.link_id.clone(),
            DependencyLookupEntry {
                name: iface.name.as_str().to_string(),
                tag: iface.tag.clone(),
                sha256: iface.sha256.clone(),
                from_any: iface.from_any,
                kind: DependencyKind::Interface,
            },
        );
    }
    out
}

/// Collects consumed interfaces from a node config and resolves their message
/// formats by looking up the exposed interfaces from dependency nodes via the
/// caller-supplied resolver.
///
/// The `resolve` closure returns a [`config::node::NodeConfig`] for a given
/// `(name, tag)` pair, or `None` if the dependency cannot be found. Callers
/// usually wrap a [`NodeStack`] (see [`stack_resolver`]) but can also chain a
/// peer map first to resolve sibling nodes that haven't been added to the
/// stack yet — used by `node sync -a` for batch operations.
pub fn collect_consumed_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    resolve: impl Fn(&str, &str) -> Option<config::node::NodeConfig>,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces = Vec::new();
    let dep_lookup = build_dependency_lookup(manifest);

    // Pre-resolve each unique node dependency into a per-dep offerings table
    // that merges native emits/exposes (origin `None`) with conformed entries
    // (origin `Some(_)`). Native wins on key collision so the consumer side
    // still addresses native producers via `SenderTarget::Node` and conformed
    // ones via `SenderTarget::Interface`.
    let mut node_dep_offerings: HashMap<(String, String), DependencyOfferings> = HashMap::new();
    // Memoized parsed interface contracts for `depends_on.interfaces` entries,
    // keyed by `(name, tag)`. SHA verification has already happened in
    // `validate_interface_dependency_specs` upstream and again per-load
    // inside `resolve_interface_doc`.
    let mut iface_dep_contracts: HashMap<(String, String), config::interface::PeppyInterface> =
        HashMap::new();

    for entry in dep_lookup.values() {
        let key = (entry.name.clone(), entry.tag.clone());
        match entry.kind {
            DependencyKind::Node => {
                if node_dep_offerings.contains_key(&key) {
                    continue;
                }
                let Some(dep_config) = resolve(&entry.name, &entry.tag) else {
                    continue;
                };
                let conformed =
                    resolve_conforms_to(&dep_config.interfaces, peppy_dirs).map_err(|e| {
                        format!(
                            "failed to resolve `conforms_to` for dependency `{}:{}`: {e}",
                            entry.name, entry.tag
                        )
                    })?;
                node_dep_offerings.insert(key, build_dependency_offerings(&dep_config, &conformed));
            }
            DependencyKind::Interface => {
                if iface_dep_contracts.contains_key(&key) {
                    continue;
                }
                let parsed = resolve_interface_doc(
                    peppy_dirs,
                    &entry.name,
                    &entry.tag,
                    entry.sha256.as_deref(),
                )?;
                iface_dep_contracts.insert(key, parsed);
            }
        }
    }

    if let Some(topic_interfaces) = &interfaces_cfg.topics
        && let Some(consumed_topics) = &topic_interfaces.consumes
    {
        for consumed_topic in consumed_topics {
            match consumed_topic {
                config::node::ConsumedTopic::Linked(linked) => {
                    let Some(entry) = dep_lookup.get(linked.link_id.as_str()) else {
                        continue;
                    };
                    let key = (entry.name.clone(), entry.tag.clone());
                    let lookup_name = linked.name.trim();
                    let (message_format, dependency) = match entry.kind {
                        DependencyKind::Node => {
                            let Some(offerings) = node_dep_offerings.get(&key) else {
                                continue;
                            };
                            let Some((message_format, origin)) = offerings.topics.get(lookup_name)
                            else {
                                continue;
                            };
                            (
                                message_format.clone(),
                                build_dependency_context_for_node(
                                    &entry.name,
                                    &entry.tag,
                                    origin.clone(),
                                    &linked.link_id,
                                    entry.from_any,
                                ),
                            )
                        }
                        DependencyKind::Interface => {
                            let Some(parsed) = iface_dep_contracts.get(&key) else {
                                continue;
                            };
                            let Some(emitted) = parsed
                                .interfaces
                                .topics
                                .iter()
                                .find(|t| t.name.trim() == lookup_name)
                            else {
                                continue;
                            };
                            let Some(message_format) = emitted.message_format.clone() else {
                                continue;
                            };
                            (
                                message_format,
                                build_dependency_context_for_interface(
                                    &entry.name,
                                    &entry.tag,
                                    &linked.link_id,
                                    entry.from_any,
                                ),
                            )
                        }
                    };
                    interfaces.push(DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
                        topic: consumed_topic.clone(),
                        message_format,
                        dependency,
                    }));
                }
                config::node::ConsumedTopic::External(external) => {
                    interfaces.push(DeploymentInterface::new(
                        InterfaceVariant::ExternalConsumedTopic {
                            name: external.name.clone(),
                            message_format: external.message_format.clone(),
                        },
                    ));
                }
            }
        }
    }

    if let Some(service_interfaces) = &interfaces_cfg.services
        && let Some(consumed_services) = &service_interfaces.consumes
    {
        for consumed_service in consumed_services {
            let Some(entry) = dep_lookup.get(&consumed_service.link_id) else {
                continue;
            };
            let key = (entry.name.clone(), entry.tag.clone());
            let lookup_name = consumed_service.name.trim();
            let (request_format, response_format, dependency) = match entry.kind {
                DependencyKind::Node => {
                    let Some(offerings) = node_dep_offerings.get(&key) else {
                        continue;
                    };
                    let Some((request_format, response_format, origin)) =
                        offerings.services.get(lookup_name)
                    else {
                        continue;
                    };
                    (
                        request_format.clone(),
                        response_format.clone(),
                        build_dependency_context_for_node(
                            &entry.name,
                            &entry.tag,
                            origin.clone(),
                            &consumed_service.link_id,
                            entry.from_any,
                        ),
                    )
                }
                DependencyKind::Interface => {
                    let Some(parsed) = iface_dep_contracts.get(&key) else {
                        continue;
                    };
                    let Some(exposed) = parsed
                        .interfaces
                        .services
                        .iter()
                        .find(|s| s.name.trim() == lookup_name)
                    else {
                        continue;
                    };
                    let Some(request_format) = exposed.request_message_format.clone() else {
                        continue;
                    };
                    let Some(response_format) = exposed.response_message_format.clone() else {
                        continue;
                    };
                    (
                        request_format,
                        response_format,
                        build_dependency_context_for_interface(
                            &entry.name,
                            &entry.tag,
                            &consumed_service.link_id,
                            entry.from_any,
                        ),
                    )
                }
            };
            interfaces.push(DeploymentInterface::new(
                InterfaceVariant::ConsumedService {
                    service: consumed_service.clone(),
                    request_format,
                    response_format,
                    dependency,
                },
            ));
        }
    }

    if let Some(action_interfaces) = &interfaces_cfg.actions
        && let Some(consumed_actions) = &action_interfaces.consumes
    {
        for consumed_action in consumed_actions {
            let Some(entry) = dep_lookup.get(&consumed_action.link_id) else {
                continue;
            };
            let key = (entry.name.clone(), entry.tag.clone());
            let lookup_name = consumed_action.name.trim();
            let (action_message, dependency) = match entry.kind {
                DependencyKind::Node => {
                    let Some(offerings) = node_dep_offerings.get(&key) else {
                        continue;
                    };
                    let Some((action_message, origin)) = offerings.actions.get(lookup_name) else {
                        continue;
                    };
                    (
                        action_message.clone(),
                        build_dependency_context_for_node(
                            &entry.name,
                            &entry.tag,
                            origin.clone(),
                            &consumed_action.link_id,
                            entry.from_any,
                        ),
                    )
                }
                DependencyKind::Interface => {
                    let Some(parsed) = iface_dep_contracts.get(&key) else {
                        continue;
                    };
                    let Some(exposed) = parsed
                        .interfaces
                        .actions
                        .iter()
                        .find(|a| a.name.trim() == lookup_name)
                    else {
                        continue;
                    };
                    (
                        action_message_from_exposed(exposed),
                        build_dependency_context_for_interface(
                            &entry.name,
                            &entry.tag,
                            &consumed_action.link_id,
                            entry.from_any,
                        ),
                    )
                }
            };
            interfaces.push(DeploymentInterface::new(InterfaceVariant::ConsumedAction {
                action: consumed_action.clone(),
                messages: action_message,
                dependency,
            }));
        }
    }

    Ok(interfaces)
}

/// Builds a [`generator::DependencyContext`] for a `depends_on.nodes`
/// resolution path. `origin` carries the optional `(iface_name,
/// iface_tag)` when the producer node `conforms_to` an interface; `None`
/// means the producer emits natively.
fn build_dependency_context_for_node(
    dep_name: &str,
    dep_tag: &str,
    origin: Option<generator::InterfaceOrigin>,
    link_id: &str,
    from_any: bool,
) -> generator::DependencyContext {
    let ctx = match origin {
        Some(o) => generator::DependencyContext::conformed(dep_name, dep_tag, o),
        None => generator::DependencyContext::native(dep_name, dep_tag),
    };
    ctx.with_link_id(Some(link_id.to_string()))
        .with_from_any(from_any)
}

/// Builds a [`generator::DependencyContext`] for a
/// `depends_on.interfaces` resolution path. `node_name` / `node_tag`
/// carry the interface's `(name, tag)` here (no producer node is
/// involved).
fn build_dependency_context_for_interface(
    iface_name: &str,
    iface_tag: &str,
    link_id: &str,
    from_any: bool,
) -> generator::DependencyContext {
    generator::DependencyContext::interface(iface_name, iface_tag)
        .with_link_id(Some(link_id.to_string()))
        .with_from_any(from_any)
}

/// What a single dependency can provide to consumers — its native
/// emits/exposes merged with the topics/services/actions it pulls in via
/// `conforms_to`. Native entries store `None` as origin; conformed entries
/// carry the `(iface_name, iface_tag)` origin so the consumer can address the
/// producer via `SenderTarget::Interface`. Keys are trimmed names, matching
/// the consumer side's `.trim()` comparisons.
struct DependencyOfferings {
    topics: HashMap<
        String,
        (
            config::node::MessageFormat,
            Option<generator::InterfaceOrigin>,
        ),
    >,
    services: HashMap<
        String,
        (
            config::node::MessageFormat,
            config::node::MessageFormat,
            Option<generator::InterfaceOrigin>,
        ),
    >,
    actions: HashMap<String, (ConsumedActionMessage, Option<generator::InterfaceOrigin>)>,
}

fn build_dependency_offerings(
    dep_config: &config::node::NodeConfig,
    conformed: &[DeploymentInterface],
) -> DependencyOfferings {
    let mut topics: HashMap<
        String,
        (
            config::node::MessageFormat,
            Option<generator::InterfaceOrigin>,
        ),
    > = HashMap::new();
    let mut services: HashMap<
        String,
        (
            config::node::MessageFormat,
            config::node::MessageFormat,
            Option<generator::InterfaceOrigin>,
        ),
    > = HashMap::new();
    let mut actions: HashMap<String, (ConsumedActionMessage, Option<generator::InterfaceOrigin>)> =
        HashMap::new();

    // Native side first — native entries win on key collision.
    if let Some(topic_ifaces) = &dep_config.interfaces.topics
        && let Some(emits) = &topic_ifaces.emits
    {
        for emitted in emits {
            if let Some(fmt) = &emitted.message_format {
                topics
                    .entry(emitted.name.trim().to_string())
                    .or_insert_with(|| (fmt.clone(), None));
            }
        }
    }
    if let Some(service_ifaces) = &dep_config.interfaces.services
        && let Some(exposes) = &service_ifaces.exposes
    {
        for exposed in exposes {
            services
                .entry(exposed.name.trim().to_string())
                .or_insert_with(|| {
                    (
                        exposed.request_message_format.clone().unwrap_or_default(),
                        exposed.response_message_format.clone().unwrap_or_default(),
                        None,
                    )
                });
        }
    }
    if let Some(action_ifaces) = &dep_config.interfaces.actions
        && let Some(exposes) = &action_ifaces.exposes
    {
        for exposed in exposes {
            actions
                .entry(exposed.name.trim().to_string())
                .or_insert_with(|| (action_message_from_exposed(exposed), None));
        }
    }

    // Conformed side fills only the gaps left by native.
    for iface in conformed {
        match iface.interface() {
            InterfaceVariant::EmittedTopic { topic, origin } => {
                if let Some(fmt) = &topic.message_format {
                    topics
                        .entry(topic.name.trim().to_string())
                        .or_insert_with(|| (fmt.clone(), origin.clone()));
                }
            }
            InterfaceVariant::ExposedService { service, origin } => {
                services
                    .entry(service.name.trim().to_string())
                    .or_insert_with(|| {
                        (
                            service.request_message_format.clone().unwrap_or_default(),
                            service.response_message_format.clone().unwrap_or_default(),
                            origin.clone(),
                        )
                    });
            }
            InterfaceVariant::ExposedAction { action, origin } => {
                actions
                    .entry(action.name.trim().to_string())
                    .or_insert_with(|| (action_message_from_exposed(action), origin.clone()));
            }
            _ => {}
        }
    }

    DependencyOfferings {
        topics,
        services,
        actions,
    }
}

fn action_message_from_exposed(
    exposed_action: &config::node::ExposedAction,
) -> ConsumedActionMessage {
    ConsumedActionMessage {
        goal_request: exposed_action
            .goal_service
            .as_ref()
            .and_then(|s| s.request_message_format.clone()),
        goal_response: exposed_action
            .goal_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
        feedback: exposed_action
            .feedback_topic
            .as_ref()
            .and_then(|t| t.message_format.clone()),
        result_request: exposed_action
            .result_service
            .as_ref()
            .and_then(|s| s.request_message_format.clone()),
        result_response: exposed_action
            .result_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
    }
}

/// Resolves every `interfaces.conforms_to` entry against the local interface
/// cache and returns the pulled interface's topics/services/actions as a
/// `Vec<DeploymentInterface>` ready to feed [`generator::generate_peppygen_lib`].
///
/// Each returned `DeploymentInterface` is stamped with an
/// [`InterfaceOrigin`] so the generator nests it under
/// `emitted_topics/{iface_name}/{iface_tag}/{leaf}` (and similar for services
/// and actions) and embeds the matching `(iface_name, iface_tag)` segments in
/// the generated wire-path calls.
///
/// Errors:
/// - Duplicate raw `(name, tag)` entries (sha256 differences do not count).
/// - Two entries that sanitize to the same `(iface_name, iface_tag)` — e.g.
///   `v1` and `v-1` collide because the wire-path tag normalization replaces
///   hyphens with underscores. Refusing this keeps generated symbols
///   addressable without ambiguity.
/// - Cache miss — surfaces "run `peppy repo refresh`".
/// - `sha256` pin set but the on-disk content has drifted.
/// Loads a `PeppyInterface` document from the local interface cache for
/// `(name, tag)`, verifying both the SHA pin (when set) and on-disk drift
/// against the cached fingerprint. Returns the parsed interface document
/// alongside the cache entry's path, so callers can pass through to the
/// `InterfaceOrigin` stamping step. Shared between [`resolve_conforms_to`]
/// (producer side) and the `depends_on.interfaces` resolution path
/// (consumer side).
pub(crate) fn resolve_interface_doc(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
) -> std::result::Result<config::interface::PeppyInterface, String> {
    let cache = repo_cache::load_interface_cache(peppy_dirs)
        .map_err(|e| format!("failed to load interface cache: {e}"))?;

    let entry = match sha256_pin {
        Some(sha) => {
            repo_cache::lookup_interface_by_sha256(&cache, name, tag, sha).ok_or_else(|| {
                format!(
                    "interface `{name}:{tag}` (sha256 `{sha}`) not in interface cache; \
                     run `peppy repo refresh`"
                )
            })?
        }
        None => repo_cache::lookup_interface(&cache, name, tag).ok_or_else(|| {
            format!("interface `{name}:{tag}` not in interface cache; run `peppy repo refresh`")
        })?,
    };

    let bytes = std::fs::read(&entry.path).map_err(|e| {
        format!(
            "failed to read cached interface `{name}:{tag}` at {}: {e}",
            entry.path
        )
    })?;
    let actual_sha = config::fingerprint::fingerprint_for_bytes(&bytes);
    if actual_sha != entry.sha256 {
        return Err(format!(
            "interface `{name}:{tag}` content drifted from cache fingerprint \
             (expected `{}`, got `{actual_sha}`); run `peppy repo refresh`",
            entry.sha256
        ));
    }

    let content = std::str::from_utf8(&bytes)
        .map_err(|e| format!("cached interface `{name}:{tag}` is not UTF-8: {e}"))?;
    config::interface::PeppyInterfaceParser::from_content(content)
        .map_err(|e| format!("failed to parse cached interface `{name}:{tag}`: {e}"))
}

/// Validates that every `depends_on.interfaces` entry on the manifest can
/// be loaded from the local interface cache (and that pinned SHAs match
/// and on-disk content has not drifted). Runs early in the sync pipeline
/// so deployments fail fast with a clear pointer to `peppy repo refresh`
/// instead of crashing deep in the resolver.
pub fn validate_interface_dependency_specs(
    manifest: &config::node::Manifest,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<(), String> {
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return Ok(());
    };
    for entry in &depends_on.interfaces {
        resolve_interface_doc(
            peppy_dirs,
            entry.name.as_str(),
            entry.tag.as_str(),
            entry.sha256.as_deref(),
        )?;
    }
    Ok(())
}

pub fn resolve_conforms_to(
    interfaces_cfg: &config::node::Interfaces,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let Some(items) = interfaces_cfg.conforms_to.as_ref() else {
        return Ok(Vec::new());
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // Sanitized-key collisions strictly dominate raw-key duplicates (an exact
    // raw dup collides post-sanitize too), so one pass catches both. Compare
    // the prior raw tag to distinguish "duplicate" from "collides after
    // hyphen→underscore normalization" (e.g. `v1` vs `v-1`) — both would
    // generate to the same module path and wire segments.
    let mut seen: HashMap<(String, String), String> = HashMap::new();
    for item in items {
        let sanitized_tag = item.tag.replace('-', "_");
        let key = (item.name.as_str().to_string(), sanitized_tag);
        if let Some(prior_tag) = seen.insert(key, item.tag.clone()) {
            if prior_tag == item.tag {
                return Err(format!(
                    "duplicate `conforms_to` entry `{}:{}`",
                    item.name.as_str(),
                    item.tag
                ));
            }
            return Err(format!(
                "`conforms_to` entries `{}:{}` and `{}:{}` collide after \
                 tag normalization (hyphens become underscores); rename one \
                 to disambiguate",
                item.name.as_str(),
                prior_tag,
                item.name.as_str(),
                item.tag
            ));
        }
    }

    let mut out: Vec<DeploymentInterface> = Vec::new();
    for item in items {
        let name = item.name.as_str();
        let tag = item.tag.as_str();
        let parsed = resolve_interface_doc(peppy_dirs, name, tag, item.sha256.as_deref())?;

        let origin = InterfaceOrigin {
            iface_name: name.to_string(),
            iface_tag: tag.to_string(),
        };

        for topic in parsed.interfaces.topics {
            out.push(DeploymentInterface::new(InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(origin.clone()),
            }));
        }
        for service in parsed.interfaces.services {
            out.push(DeploymentInterface::new(InterfaceVariant::ExposedService {
                service,
                origin: Some(origin.clone()),
            }));
        }
        for action in parsed.interfaces.actions {
            out.push(DeploymentInterface::new(InterfaceVariant::ExposedAction {
                action,
                origin: Some(origin.clone()),
            }));
        }
    }

    Ok(out)
}

/// Convenience helper that builds a resolver closure backed by a [`NodeStack`].
///
/// Use this for callers that don't have any local peers to layer on top of
/// the daemon's persistent stack — i.e. `node add` and `auto_sync_if_missing`.
pub fn stack_resolver(
    node_stack: &NodeStack,
) -> impl Fn(&str, &str) -> Option<config::node::NodeConfig> + '_ {
    move |name, tag| {
        node_stack
            .find(name, tag)
            .map(|e| e.read().config().clone())
    }
}

/// Parameters for [`auto_sync_if_missing`].
pub struct AutoSyncParams<'a> {
    pub node_dir: &'a std::path::Path,
    pub execution_language: config::node::PeppygenLanguage,
    pub manifest: &'a config::node::Manifest,
    pub interfaces: &'a config::node::Interfaces,
    pub git_hash: &'a str,
}

/// Auto-generates the `.peppy` directory for a node that has never been synced.
///
/// When the `.peppy` directory is entirely absent (e.g. fresh clone), this
/// function generates peppygen.
///
/// Directories whose `.peppy` already exists and contains all required files
/// are skipped (no-op). If `.peppy` exists but is incomplete (e.g. missing
/// `git.hash` or the peppygen fingerprint), it is removed and regenerated.
pub fn auto_sync_if_missing(
    params: AutoSyncParams<'_>,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> crate::Result<()> {
    let peppy_dir = params.node_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if needs_sync(params.node_dir) {
        // Back up existing .peppy so we can restore it on failure.
        let backup_dir = params.node_dir.join(format!(
            ".peppy-backup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let had_backup = match std::fs::rename(&peppy_dir, &backup_dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(crate::Error::Io(e)),
        };

        let gen_result: crate::Result<()> = (|| {
            let mut consumed = collect_consumed_interfaces(
                params.manifest,
                params.interfaces,
                stack_resolver(node_stack),
                peppy_dirs,
            )
            .map_err(|reason| {
                crate::Error::Io(std::io::Error::other(format!(
                    "failed to resolve consumed interfaces: {}",
                    reason
                )))
            })?;
            let conformed =
                resolve_conforms_to(params.interfaces, peppy_dirs).map_err(|reason| {
                    crate::Error::Io(std::io::Error::other(format!(
                        "failed to resolve `conforms_to` interfaces: {}",
                        reason
                    )))
                })?;
            consumed.extend(conformed);
            generate_peppygen_for_node(
                params.execution_language,
                params.node_dir,
                consumed,
                params.git_hash,
                peppy_dirs,
                generator::CrateDeployMode::default(),
                None,
            )?;
            Ok(())
        })();

        match gen_result {
            Ok(()) => {
                // Clean up the backup synchronously. We *must not* defer this
                // to a background thread: the next stage (`process_node_add`)
                // copies the source directory recursively, walks
                // `.peppy-backup-PID-NANOS` (which is not in the excluded
                // list), and would race with a concurrent deletion — surfacing
                // as intermittent "No such file or directory" errors.
                //
                // A failure here must be surfaced rather than silently
                // ignored: leaving the backup behind would still trip the
                // recursive copy described above.
                if had_backup {
                    std::fs::remove_dir_all(&backup_dir).map_err(|e| {
                        crate::Error::Io(std::io::Error::other(format!(
                            "failed to clean up .peppy backup at {}: {}",
                            backup_dir.display(),
                            e
                        )))
                    })?;
                }
            }
            Err(e) => {
                // Generation failed — remove partial .peppy and restore backup.
                let _ = std::fs::remove_dir_all(&peppy_dir);
                if had_backup && let Err(restore_err) = std::fs::rename(&backup_dir, &peppy_dir) {
                    tracing::error!(
                        "Failed to restore .peppy backup from {}: {}",
                        backup_dir.display(),
                        restore_err,
                    );
                }
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Generates the peppygen library for a node.
///
/// This function takes the pre-collected data and generates the peppygen
/// library in the node directory. Use `collect_consumed_interfaces` to
/// gather the consumed interfaces before calling this function.
///
/// This function is designed to be called from within `spawn_blocking` contexts
/// where the data has already been extracted and can be moved into the closure.
pub fn generate_peppygen_for_node(
    language: config::node::PeppygenLanguage,
    node_dir: impl AsRef<std::path::Path>,
    consumed_interfaces: Vec<DeploymentInterface>,
    git_hash: &str,
    peppy_dirs: &PeppyDirs,
    deploy_mode: generator::CrateDeployMode,
    config_path: Option<&std::path::Path>,
) -> crate::Result<()> {
    generator::generate_peppygen_lib(
        language,
        node_dir,
        consumed_interfaces,
        git_hash,
        peppy_dirs,
        deploy_mode,
        config_path,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_sync_returns_true_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".peppy")).unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"").unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_true_when_fingerprint_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        assert!(needs_sync(tmp.path()));
    }

    #[test]
    fn needs_sync_returns_false_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        let peppygen = peppy.join("libs/peppygen");
        std::fs::create_dir_all(&peppygen).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        std::fs::write(peppygen.join("peppy.json5.sha256"), b"deadbeef").unwrap();
        assert!(!needs_sync(tmp.path()));
    }
}

#[cfg(test)]
mod conforms_to_tests {
    //! Exercises [`resolve_conforms_to`]: the cache-loading side of
    //! `interfaces.conforms_to` resolution. The generator-side (module
    //! nesting / wire-segment embedding) is verified by the integration
    //! tests in `crates/generator-internal/tests/{rust,python}/conforms_to.rs`.

    use super::*;
    use config::node::{ConformsToItem, Interfaces, Name};
    use core_node_api::encoding::RepoSourceKind;
    use std::fs;
    use tempfile::TempDir;

    /// Writes an interface manifest to `dir/{name}_{tag}.json5` and seeds the
    /// returned `InterfaceCacheEntry` with the matching sha256. Returns
    /// `(entry, abs_path)` so callers can either keep or mutate the entry
    /// (e.g. for the drift test).
    fn seed_interface(
        dir: &std::path::Path,
        name: &str,
        tag: &str,
        body: &str,
    ) -> repo_cache::InterfaceCacheEntry {
        let file_name = format!("{name}_{tag}.json5");
        let path = dir.join(&file_name);
        fs::write(&path, body).expect("write interface file");
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        repo_cache::InterfaceCacheEntry {
            interface_name: name.to_string(),
            tag: tag.to_string(),
            sha256: sha,
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            path: path.to_string_lossy().to_string(),
            repo_id: 0,
        }
    }

    /// Builds an interfaces.json5 cache + dir-rooted `PeppyDirs` from a set of
    /// seeded entries.
    fn make_peppy_dirs_with_cache(
        entries: &[repo_cache::InterfaceCacheEntry],
    ) -> (TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let dirs = PeppyDirs::new(tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");
        let cache_path = repo_cache::interfaces_repo_cache_path(&dirs);
        let json = serde_json5::to_string(&entries.to_vec()).expect("serialize cache");
        fs::write(&cache_path, json).expect("write cache file");
        (tmp, dirs)
    }

    const DEPTH_V1_BODY: &str = r#"{
        peppy_schema: "interface_v1",
        manifest: { name: "depth_camera", tag: "v1" },
        interfaces: {
            topics: [
                { name: "video_stream", qos_profile: "sensor_data" }
            ]
        }
    }"#;

    fn interfaces_with_conforms(items: Vec<ConformsToItem>) -> Interfaces {
        Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: Some(items),
        }
    }

    #[test]
    fn returns_empty_when_no_conforms_to() {
        let dirs = PeppyDirs::new(TempDir::new().unwrap().path().to_path_buf());
        let cfg = Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: None,
        };
        let out = resolve_conforms_to(&cfg, &dirs).expect("ok");
        assert!(out.is_empty());
    }

    /// Happy path: a `conforms_to` entry whose `(name, tag)` is present in the
    /// interfaces cache yields the underlying interface's topics, each wrapped
    /// as `EmittedTopic` and stamped with `origin` pointing back to the source
    /// interface so downstream codegen can attribute the topic.
    #[test]
    fn resolves_cache_hit_with_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_interface(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let out = resolve_conforms_to(&cfg, &dirs).expect("happy path");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(o.iface_name, "depth_camera");
                assert_eq!(o.iface_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    #[test]
    fn cache_miss_suggests_repo_refresh() {
        // Empty cache — any lookup misses.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);
        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let err = resolve_conforms_to(&cfg, &dirs).expect_err("miss must error");
        assert!(
            err.contains("`depth_camera:v1`") && err.contains("peppy repo refresh"),
            "missing-from-cache error should name the entry and suggest refresh, got: {err}"
        );
    }

    #[test]
    fn duplicate_raw_entries_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_interface(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        // Two entries with the same raw `(name, tag)` — sha256 differing
        // should NOT rescue this case per the spec.
        let cfg = interfaces_with_conforms(vec![
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v1".to_string(),
                sha256: None,
            },
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v1".to_string(),
                sha256: Some("aaa".to_string()),
            },
        ]);

        let err = resolve_conforms_to(&cfg, &dirs).expect_err("dup must error");
        assert!(
            err.contains("duplicate") && err.contains("depth_camera:v1"),
            "duplicate error should name the entry, got: {err}"
        );
    }

    #[test]
    fn tag_sanitize_collisions_are_rejected() {
        // `v_1` and `v-1` both sanitize to `v_1` after the hyphen→underscore
        // pass that the wire-path and generated-symbol layers apply. Refuse
        // rather than silently merge.
        let tmp = TempDir::new().unwrap();
        let entry_a = seed_interface(tmp.path(), "depth_camera", "v_1", DEPTH_V1_BODY);
        let body_b = DEPTH_V1_BODY.replace("\"v1\"", "\"v-1\"");
        let entry_b = seed_interface(tmp.path(), "depth_camera", "v-1", &body_b);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry_a, entry_b]);

        let cfg = interfaces_with_conforms(vec![
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v_1".to_string(),
                sha256: None,
            },
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v-1".to_string(),
                sha256: None,
            },
        ]);

        let err = resolve_conforms_to(&cfg, &dirs).expect_err("collision must error");
        assert!(
            err.contains("collide") && err.contains("normalization"),
            "sanitize-collision error should mention collision + normalization, got: {err}"
        );
    }

    const ARM_V1_WITH_SERVICE_AND_ACTION: &str = r#"{
        peppy_schema: "interface_v1",
        manifest: { name: "arm", tag: "v1" },
        interfaces: {
            services: [
                { name: "control" }
            ],
            actions: [
                { name: "move_arm" }
            ]
        }
    }"#;

    /// A `conforms_to` entry whose body declares a service AND an action must
    /// yield both as `ExposedService`/`ExposedAction` variants stamped with
    /// `Some(origin)` pointing back at the source interface. Mirrors
    /// `resolves_cache_hit_with_origin` but exercises the non-topic variants.
    #[test]
    fn resolves_cache_hit_with_service_and_action_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_interface(tmp.path(), "arm", "v1", ARM_V1_WITH_SERVICE_AND_ACTION);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("arm").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let out = resolve_conforms_to(&cfg, &dirs).expect("happy path");

        let mut saw_service = false;
        let mut saw_action = false;
        for entry in &out {
            match entry.interface() {
                InterfaceVariant::ExposedService {
                    service,
                    origin: Some(o),
                } => {
                    assert_eq!(service.name, "control");
                    assert_eq!(o.iface_name, "arm");
                    assert_eq!(o.iface_tag, "v1");
                    saw_service = true;
                }
                InterfaceVariant::ExposedAction {
                    action,
                    origin: Some(o),
                } => {
                    assert_eq!(action.name, "move_arm");
                    assert_eq!(o.iface_name, "arm");
                    assert_eq!(o.iface_tag, "v1");
                    saw_action = true;
                }
                other => panic!("unexpected resolved variant: {other:?}"),
            }
        }
        assert!(saw_service, "service should be resolved with origin");
        assert!(saw_action, "action should be resolved with origin");
    }

    #[test]
    fn sha256_drift_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_interface(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        // Rewrite the underlying file so its fingerprint no longer matches
        // the cache's `sha256` — i.e. the cache thinks the file is X but it
        // is now Y. resolve_conforms_to must catch this.
        fs::write(
            &entry.path,
            DEPTH_V1_BODY.replace("video_stream", "video_stream_v2"),
        )
        .unwrap();
        // Keep the stale (pre-rewrite) sha256 in the cache entry. We need to
        // ensure load_interface_cache trusts it.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);
        let err = resolve_conforms_to(&cfg, &dirs).expect_err("drift must error");
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }
}
