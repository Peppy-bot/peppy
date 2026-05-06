use crate::Result;
use crate::names;
use crate::services::node::cache as node_cache;
use crate::services::repo::cache as repo_cache;
use config::consts::PeppyDirs;
use config::node::{NodeConfigParser, VariantConfigParser};
use config::source::DeploymentSource;
use core_node_api::encoding::{NodeSyncRequest, NodeSyncResponse, RepoResolvedEntry};
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceVariant};
use node_stack::NodeStack;
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
        node_name,
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
/// 2. Spawns background cleanup of the old directory
/// 3. Lets the generator create a fresh `.peppy` directory
fn remove_previous_peppy_dir(node_root_dir: &std::path::Path) {
    let peppy_output_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);

    // Safety checks: ensure we're only operating on a `.peppy` directory
    if !peppy_output_dir.exists() {
        return;
    }
    if !peppy_output_dir.is_dir() {
        debug!(
            "Expected .peppy to be a directory, but it's a file: {}",
            peppy_output_dir.display()
        );
        return;
    }
    if peppy_output_dir.file_name() != Some(std::ffi::OsStr::new(config::consts::PEPPY_OUTPUT_DIR))
    {
        debug!(
            "Unexpected directory name, expected {}: {}",
            config::consts::PEPPY_OUTPUT_DIR,
            peppy_output_dir.display()
        );
        return;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let old_peppy_dir =
        node_root_dir.join(format!(".peppy-old-{}-{}", std::process::id(), timestamp));

    match std::fs::rename(&peppy_output_dir, &old_peppy_dir) {
        Ok(()) => {
            // Best-effort cleanup of old directory in background
            std::thread::spawn(move || {
                std::fs::remove_dir_all(&old_peppy_dir).ok();
            });
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
/// - `libs/peppygen/peppy.json5.sha256` (when `has_execution_language` is true)
fn needs_sync(node_root_dir: &std::path::Path, has_execution_language: bool) -> bool {
    let peppy_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if !peppy_dir.exists() {
        return true;
    }

    // git.hash must be a regular non-empty file
    match std::fs::metadata(peppy_dir.join("git.hash")) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => {}
        _ => return true,
    }

    // fingerprint file required when execution language is present;
    // must be a regular non-empty file
    if has_execution_language {
        match std::fs::metadata(peppy_dir.join("libs/peppygen/peppy.json5.sha256")) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => {}
            _ => return true,
        }
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
        match materialize_repo_deps(parsed.manifest(), node_stack, &peppy_dirs).await {
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
    let (
        consumed_interfaces,
        language,
        variants,
        root_manifest,
        root_interfaces,
        root_peppy_schema,
    ) = if !node_config_path.exists() {
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
                    node_config.manifest(),
                    node_config.interfaces(),
                    node_config.manifest_name(),
                    node_config.manifest_tag(),
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
                        node_stack::NodeStackError::UndeclaredLocalNodeId {
                            local_node_id, ..
                        } => {
                            missing_interfaces.push(format!(
                                "references undeclared local_node_id `{}`",
                                local_node_id
                            ));
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
                        node_config.manifest_name(),
                        node_config.manifest_tag(),
                        errors.join("; ")
                    ))
                    .encode()
                    .map_err(Into::into);
                }

                // Collect consumed interfaces with resolved message formats
                let interfaces = match collect_consumed_interfaces(
                    node_config.manifest(),
                    node_config.interfaces(),
                    resolve_dep,
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
                let language = node_config.execution_language();
                let variants = node_config.manifest().variants.clone();
                let root_manifest = node_config.manifest().clone();
                let root_interfaces = node_config.interfaces().clone();
                let root_peppy_schema = node_config.peppy_schema();
                (
                    interfaces,
                    language,
                    variants,
                    root_manifest,
                    root_interfaces,
                    root_peppy_schema,
                )
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

    let node_root_dir_for_variants = node_root_dir.clone();
    let git_hash_for_variants = git_hash.clone();
    let consumed_interfaces_for_variants = consumed_interfaces.clone();
    let peppy_dirs_for_variants = peppy_dirs.clone();

    // Generate peppygen for the root node (skip if execution is absent, e.g. default-variant nodes).
    if let Some(language) = language {
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
    } else {
        // Variant-only node: no peppygen at root, but .peppy/git.hash must
        // still exist alongside the manifest so that `node add` can verify
        // the source is in sync.
        remove_previous_peppy_dir(&node_root_dir);
        let peppy_dir = node_root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
        if let Err(e) = std::fs::create_dir_all(&peppy_dir)
            .and_then(|()| std::fs::write(peppy_dir.join("git.hash"), git_hash.as_bytes()))
        {
            return NodeSyncResponse::failure(format!(
                "Failed to write git hash at root for variant-only node: {}",
                e
            ))
            .encode()
            .map_err(Into::into);
        }
    }

    // Sync variants: each variant gets its own .peppy directory using the root's interfaces
    if let Some(variants) = variants {
        for variant in &variants {
            let DeploymentSource::Local(local) = &variant.source else {
                debug!(
                    "Skipping non-local variant '{}' during sync",
                    variant.name.as_str()
                );
                continue;
            };

            let variant_dir = if local.local.is_relative() {
                node_root_dir_for_variants.join(&local.local)
            } else {
                local.local.clone()
            };
            let variant_dir = variant_dir.canonicalize().unwrap_or(variant_dir);

            if !variant_dir.exists() {
                return NodeSyncResponse::failure(format!(
                    "Variant '{}' source directory does not exist: {}",
                    variant.name.as_str(),
                    variant_dir.display()
                ))
                .encode()
                .map_err(Into::into);
            }

            let variant_config_path = variant_dir.join(config::consts::NODE_CONFIG_FILE);
            let variant_config = match VariantConfigParser::from_path(&variant_config_path) {
                Ok(vc) => vc,
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to parse variant '{}' config: {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode()
                    .map_err(Into::into);
                }
            };

            let variant_language = variant_config.execution.language;

            // Write a merged NodeConfig (root manifest + root interfaces + variant execution)
            // so the generator can read it as a standard NodeConfig.
            // Strip the variants list — it is no longer relevant once resolved and
            // would trigger a validation error with a "default" variant + execution.
            let mut merged_manifest = root_manifest.clone();
            merged_manifest.variants = None;
            let merged_config = config::node::NodeConfig {
                peppy_schema: root_peppy_schema,
                manifest: merged_manifest,
                interfaces: root_interfaces.clone(),
                execution: variant_config.execution,
            };
            let merged_json5 = match serde_json5::to_string(&merged_config) {
                Ok(json) => json,
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to serialize merged config for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode()
                    .map_err(Into::into);
                }
            };
            // Write the merged config to a temporary file so the generator can
            // read a full NodeConfig without overwriting the user's peppy.json5.
            let variant_merged_config_file = match tempfile::Builder::new()
                .prefix(".peppy-merged-")
                .suffix(".json5")
                .tempfile()
            {
                Ok(mut f) => {
                    if let Err(e) = std::io::Write::write_all(&mut f, merged_json5.as_bytes()) {
                        return NodeSyncResponse::failure(format!(
                            "Failed to write merged config for variant '{}': {}",
                            variant.name.as_str(),
                            e
                        ))
                        .encode()
                        .map_err(Into::into);
                    }
                    f
                }
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to create temp file for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode()
                    .map_err(Into::into);
                }
            };
            let variant_merged_config_path = variant_merged_config_file.path().to_path_buf();

            let variant_interfaces = consumed_interfaces_for_variants.clone();
            let variant_git_hash = git_hash_for_variants.clone();
            let variant_peppy_dirs = peppy_dirs_for_variants.clone();

            match tokio::task::spawn_blocking(move || -> Result<()> {
                // Keep the temp file alive for the duration of generation;
                // it is automatically deleted when `_merged_config` is dropped.
                let _merged_config = variant_merged_config_file;
                remove_previous_peppy_dir(&variant_dir);
                generate_peppygen_for_node(
                    variant_language,
                    &variant_dir,
                    variant_interfaces,
                    &variant_git_hash,
                    &variant_peppy_dirs,
                    generator::CrateDeployMode::default(),
                    Some(&variant_merged_config_path),
                )?;
                // Re-fingerprint using the variant's own peppy.json5 so that
                // node-add verification (which reads the variant's config) finds
                // a matching hash.
                config::fingerprint::generate_node_config_fingerprint(
                    variant_dir.join(config::consts::NODE_CONFIG_FILE),
                    variant_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH),
                )
                .map_err(generator::GeneratorError::Config)?;
                Ok(())
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to generate peppygen for variant '{}': {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode()
                    .map_err(Into::into);
                }
                Err(e) => {
                    return NodeSyncResponse::failure(format!(
                        "Failed to generate peppygen for variant '{}' (task failed): {}",
                        variant.name.as_str(),
                        e
                    ))
                    .encode()
                    .map_err(Into::into);
                }
            }
        }
    }

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
                    .map_err(|e| format!("failed to load packages cache: {e}"))?,
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
        if let Some(child_deps) = parsed.manifest().depends_on.as_ref() {
            for child in &child_deps.nodes {
                let key = (child.name.as_str().to_owned(), child.tag.clone());
                if !seen.contains(&key) {
                    pending.push(key);
                }
            }
        }

        let cfg = parsed.into_resolved_or_default();
        resolved.insert((name.clone(), tag.clone()), cfg);
        provenance.push(RepoResolvedEntry {
            name,
            tag,
            source_kind,
        });
    }

    Ok((resolved, provenance, stack_hits))
}

/// Builds a lookup from `local_id` → `(dep_name, dep_tag)` using the node's `depends_on.nodes`.
fn build_dependency_lookup(
    manifest: &config::node::Manifest,
) -> std::collections::HashMap<String, (String, String)> {
    manifest
        .depends_on
        .as_ref()
        .map(|d| {
            d.nodes
                .iter()
                .map(|n| {
                    (
                        n.local_id.clone(),
                        (n.name.as_str().to_string(), n.tag.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
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
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces = Vec::new();
    let dep_lookup = build_dependency_lookup(manifest);

    // Pre-resolve each unique (name, tag) into a cloned NodeConfig so the
    // per-item loops below don't repeatedly hit the resolver (which may take
    // a NodeStack read lock or do filesystem I/O).
    let mut dep_configs: std::collections::HashMap<(String, String), config::node::NodeConfig> =
        std::collections::HashMap::new();
    for (dep_name, dep_tag) in dep_lookup.values() {
        let key = (dep_name.clone(), dep_tag.clone());
        if !dep_configs.contains_key(&key)
            && let Some(cfg) = resolve(dep_name, dep_tag)
        {
            dep_configs.insert(key, cfg);
        }
    }

    // Collect consumed topics
    if let Some(topic_interfaces) = &interfaces_cfg.topics
        && let Some(consumed_topics) = &topic_interfaces.consumes
    {
        for consumed_topic in consumed_topics {
            match consumed_topic {
                config::node::ConsumedTopic::Linked(linked) => {
                    let Some((dep_name, dep_tag)) = dep_lookup.get(linked.local_node_id.as_str())
                    else {
                        continue;
                    };
                    if let Some(dep_config) = dep_configs.get(&(dep_name.clone(), dep_tag.clone()))
                        && let Some(dep_topics) = &dep_config.interfaces.topics
                        && let Some(emitted_topics) = &dep_topics.emits
                        && let Some(emitted_topic) = emitted_topics
                            .iter()
                            .find(|t| t.name.trim() == linked.name.trim())
                        && let Some(message_format) = &emitted_topic.message_format
                    {
                        interfaces.push(DeploymentInterface::new(
                            InterfaceVariant::ConsumedTopic {
                                topic: consumed_topic.clone(),
                                message_format: message_format.clone(),
                                dependency_node_name: dep_name.clone(),
                            },
                        ));
                    }
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

    // Collect consumed services
    if let Some(service_interfaces) = &interfaces_cfg.services
        && let Some(consumed_services) = &service_interfaces.consumes
    {
        for consumed_service in consumed_services {
            let Some((dep_name, dep_tag)) = dep_lookup.get(&consumed_service.local_node_id) else {
                continue;
            };
            if let Some(dep_config) = dep_configs.get(&(dep_name.clone(), dep_tag.clone()))
                && let Some(dep_services) = &dep_config.interfaces.services
                && let Some(exposed_services) = &dep_services.exposes
                && let Some(exposed_service) = exposed_services
                    .iter()
                    .find(|s| s.name.trim() == consumed_service.name.trim())
            {
                interfaces.push(DeploymentInterface::new(
                    InterfaceVariant::ConsumedService {
                        service: consumed_service.clone(),
                        request_format: exposed_service
                            .request_message_format
                            .clone()
                            .unwrap_or_default(),
                        response_format: exposed_service
                            .response_message_format
                            .clone()
                            .unwrap_or_default(),
                        dependency_node_name: dep_name.clone(),
                    },
                ));
            }
        }
    }

    // Collect consumed actions
    if let Some(action_interfaces) = &interfaces_cfg.actions
        && let Some(consumed_actions) = &action_interfaces.consumes
    {
        for consumed_action in consumed_actions {
            let Some((dep_name, dep_tag)) = dep_lookup.get(&consumed_action.local_node_id) else {
                continue;
            };
            if let Some(dep_config) = dep_configs.get(&(dep_name.clone(), dep_tag.clone()))
                && let Some(dep_actions) = &dep_config.interfaces.actions
                && let Some(exposed_actions) = &dep_actions.exposes
                && let Some(exposed_action) = exposed_actions
                    .iter()
                    .find(|a| a.name.trim() == consumed_action.name.trim())
            {
                let action_message = ConsumedActionMessage {
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
                };

                interfaces.push(DeploymentInterface::new(InterfaceVariant::ConsumedAction {
                    action: consumed_action.clone(),
                    messages: action_message,
                    dependency_node_name: dep_name.clone(),
                }));
            }
        }
    }

    Ok(interfaces)
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
    pub execution_language: Option<config::node::PeppygenLanguage>,
    pub manifest: &'a config::node::Manifest,
    pub interfaces: &'a config::node::Interfaces,
    pub git_hash: &'a str,
    pub variant: Option<AutoSyncVariant<'a>>,
}

pub struct AutoSyncVariant<'a> {
    pub dir: &'a std::path::Path,
    pub language: config::node::PeppygenLanguage,
    /// The fully merged node config (root manifest + variant execution).
    /// Needed because the variant's own `peppy.json5` lacks a `manifest`.
    pub merged_config: &'a config::node::NodeConfig,
}

/// Auto-generates the `.peppy` directory for a node that has never been synced.
///
/// When the `.peppy` directory is entirely absent (e.g. fresh clone), this
/// function generates peppygen (if the node has an execution block) or writes
/// just the `git.hash` file (variant-only nodes without execution at root).
///
/// If a variant is provided and its `.peppy` directory is also absent,
/// generates peppygen for the variant and re-fingerprints using the variant's
/// own `peppy.json5`.
///
/// Directories whose `.peppy` already exists and contains all required files
/// are skipped (no-op). If `.peppy` exists but is incomplete (e.g. missing
/// `git.hash` or the peppygen fingerprint), it is removed and regenerated.
pub fn auto_sync_if_missing(
    params: AutoSyncParams<'_>,
    node_stack: &NodeStack,
    peppy_dirs: &PeppyDirs,
) -> crate::Result<()> {
    // Sync root
    let peppy_dir = params.node_dir.join(config::consts::PEPPY_OUTPUT_DIR);
    if needs_sync(params.node_dir, params.execution_language.is_some()) {
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
            if let Some(language) = params.execution_language {
                let consumed = collect_consumed_interfaces(
                    params.manifest,
                    params.interfaces,
                    stack_resolver(node_stack),
                )
                .map_err(|reason| {
                    crate::Error::Io(std::io::Error::other(format!(
                        "failed to resolve consumed interfaces: {}",
                        reason
                    )))
                })?;
                generate_peppygen_for_node(
                    language,
                    params.node_dir,
                    consumed,
                    params.git_hash,
                    peppy_dirs,
                    generator::CrateDeployMode::default(),
                    None,
                )?;
            } else {
                // Variant-only node (no execution at root): just write git.hash
                std::fs::create_dir_all(&peppy_dir)
                    .and_then(|()| {
                        std::fs::write(peppy_dir.join("git.hash"), params.git_hash.as_bytes())
                    })
                    .map_err(crate::Error::from)?;
            }
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

    // Sync variant
    if let Some(v) = params.variant
        && needs_sync(v.dir, true)
    {
        // Write the merged config (root manifest + variant execution) to a
        // temp file so the generator can parse a full NodeConfig. The
        // variant's own peppy.json5 lacks a `manifest` field.
        // Strip variant declarations from the manifest to avoid the
        // ExecutionWithDefaultVariant validation error when the generator
        // re-parses the config.
        let mut config_for_gen = v.merged_config.clone();
        config_for_gen.manifest.variants = None;
        let merged_json5 = serde_json5::to_string(&config_for_gen)
            .map_err(|e| crate::Error::Io(std::io::Error::other(e)))?;
        // Keep the temp file alive while `merged_config_path` is in use;
        // it is automatically deleted when `tmp` is dropped.
        let mut tmp = tempfile::Builder::new()
            .prefix(".peppy-merged-")
            .suffix(".json5")
            .tempfile()
            .map_err(crate::Error::from)?;
        std::io::Write::write_all(&mut tmp, merged_json5.as_bytes()).map_err(crate::Error::from)?;
        let merged_config_path = tmp.path().to_path_buf();

        // Resolve consumed interfaces *before* touching the filesystem: an
        // error here must not leave the variant's `.peppy` dir missing.
        // Doing this after the rename would short-circuit past the
        // `gen_result` restore path below.
        let consumed = collect_consumed_interfaces(
            params.manifest,
            params.interfaces,
            stack_resolver(node_stack),
        )
        .map_err(|reason| {
            crate::Error::Io(std::io::Error::other(format!(
                "failed to resolve consumed interfaces: {}",
                reason
            )))
        })?;

        // Back up existing .peppy so we can restore it on failure.
        let peppy_dir = v.dir.join(config::consts::PEPPY_OUTPUT_DIR);
        let backup_dir = v.dir.join(format!(
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
            generate_peppygen_for_node(
                v.language,
                v.dir,
                consumed,
                params.git_hash,
                peppy_dirs,
                generator::CrateDeployMode::default(),
                Some(&merged_config_path),
            )?;

            // Re-fingerprint using the variant's own peppy.json5 so that
            // node-add verification (which reads the variant's config) finds
            // a matching hash.
            config::fingerprint::generate_node_config_fingerprint(
                v.dir.join(config::consts::NODE_CONFIG_FILE),
                v.dir.join(config::consts::PEPPYGEN_OUTPUT_PATH),
            )
            .map_err(generator::GeneratorError::Config)?;
            Ok(())
        })();

        match gen_result {
            Ok(()) => {
                // Clean up the backup synchronously — see the matching comment
                // in the root-sync branch above for why a background thread
                // would race with the subsequent recursive copy. A failure
                // here must be surfaced for the same reason.
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
        assert!(needs_sync(tmp.path(), true));
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_missing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".peppy")).unwrap();
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_git_hash_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"").unwrap();
        assert!(needs_sync(tmp.path(), false));
    }

    #[test]
    fn needs_sync_returns_true_when_fingerprint_missing_with_execution_language() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        // has_execution_language = true but no fingerprint file
        assert!(needs_sync(tmp.path(), true));
    }

    #[test]
    fn needs_sync_returns_false_when_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        let peppygen = peppy.join("libs/peppygen");
        std::fs::create_dir_all(&peppygen).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        std::fs::write(peppygen.join("peppy.json5.sha256"), b"deadbeef").unwrap();
        assert!(!needs_sync(tmp.path(), true));
    }

    #[test]
    fn needs_sync_ignores_fingerprint_when_no_execution_language() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy = tmp.path().join(".peppy");
        std::fs::create_dir_all(&peppy).unwrap();
        std::fs::write(peppy.join("git.hash"), b"abc123").unwrap();
        // No fingerprint file, but has_execution_language = false
        assert!(!needs_sync(tmp.path(), false));
    }

    #[test]
    fn auto_sync_variant_restores_peppy_on_generation_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("root_node");
        let variant_dir = root_dir.join("variants").join("default");
        std::fs::create_dir_all(&variant_dir).unwrap();

        // Root .peppy is complete so root sync is skipped.
        let root_peppy = root_dir.join(config::consts::PEPPY_OUTPUT_DIR);
        std::fs::create_dir_all(&root_peppy).unwrap();
        std::fs::write(root_peppy.join("git.hash"), b"abc123").unwrap();

        // Variant .peppy has git.hash and a marker file but NO fingerprint,
        // so needs_sync(variant_dir, true) returns true.
        let variant_peppy = variant_dir.join(config::consts::PEPPY_OUTPUT_DIR);
        std::fs::create_dir_all(variant_peppy.join("libs/peppygen")).unwrap();
        std::fs::write(variant_peppy.join("git.hash"), b"old_hash").unwrap();
        std::fs::write(variant_peppy.join("marker"), b"should_survive").unwrap();

        // Do NOT create peppy.json5 in variant_dir — the re-fingerprint
        // step reads it and will fail with NotFound, exercising the
        // backup-restore path.

        let config = config::node::NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node_v1",
                manifest: { name: "test_node", tag: "0.1.0" },
                execution: { language: "rust", run_cmd: ["sleep", "10"] },
                interfaces: {
                    topics: {
                        emits: [{
                            name: "hello",
                            qos_profile: "sensor_data",
                            message_format: { message: "string" },
                        }],
                    },
                },
            }"#,
        )
        .unwrap()
        .into_resolved()
        .unwrap();

        let peppy_dirs = PeppyDirs::new(tmp.path().join("peppy_root"));
        let node_stack = NodeStack::new(config.clone(), None, &root_dir);

        let result = auto_sync_if_missing(
            AutoSyncParams {
                node_dir: &root_dir,
                execution_language: None,
                manifest: &config.manifest,
                interfaces: &config.interfaces,
                git_hash: "new_hash",
                variant: Some(AutoSyncVariant {
                    dir: &variant_dir,
                    language: config::node::PeppygenLanguage::Rust,
                    merged_config: &config,
                }),
            },
            &node_stack,
            &peppy_dirs,
        );

        assert!(
            result.is_err(),
            "should fail because variant peppy.json5 is missing"
        );
        assert!(
            variant_peppy.join("marker").exists(),
            "sentinel should survive — old .peppy must be restored on failure"
        );
        assert_eq!(
            std::fs::read_to_string(variant_peppy.join("git.hash")).unwrap(),
            "old_hash",
            "git.hash should be the pre-failure value"
        );
    }
}
