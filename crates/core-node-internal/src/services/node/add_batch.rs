//! Batch-add pipeline for `NodeSource::RepoNode` goals.
//!
//! Resolves a `(name, tag)` target against `~/.peppy/cache/packages.json5`,
//! walks its transitive deps, materializes every node through the
//! persistent git/http caches, topologically sorts them via
//! [`VirtualDeptree`], then feeds each to [`super::add::run_node_add`].
//! Mid-batch failure rolls back every node pushed during the batch via a
//! drop guard.

use super::super::repo::cache::{self, PackageEntry};
use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::add::{NodeAddActionContext, run_node_add};
use super::cache as node_cache;
use super::{FeedbackLine, FeedbackStream, create_action_log_file};
use crate::encoding::{DepVariantOverride, NodeAddGoal, NodeAddResult, NodeSource, RepoSourceKind};
use chrono::Local;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::{NodeConfigParser, ParsedNodeConfig};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use node_stack::VirtualDeptree;
use parking_lot::Mutex as StdMutex;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};
use url::Url;

/// Entry point called from `handle_goal_request` when
/// `goal.source` is `NodeSource::RepoNode`.
pub(crate) async fn run_repo_node_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeAddResult {
    let (root_name, root_tag, dep_variant_overrides) = match &goal.source {
        NodeSource::RepoNode {
            name,
            tag,
            dep_variant_overrides,
        } => (name.clone(), tag.clone(), dep_variant_overrides.clone()),
        _ => {
            return NodeAddResult::failure(
                &log_path,
                "internal error: run_repo_node_add called with non-RepoNode source".to_owned(),
            );
        }
    };

    emit(
        &feedback_tx,
        FeedbackStream::Stdout,
        format!("Resolving {}:{} from repo cache", root_name, root_tag),
    );

    let peppy_dirs = action_context.peppy_dirs.clone();

    let entries = match cache::load(&peppy_dirs) {
        Ok(entries) => entries,
        Err(e) => {
            return fail(
                &log_file,
                &log_path,
                format!("Failed to read packages cache: {}", e),
            );
        }
    };
    if entries.is_empty() {
        return fail(
            &log_file,
            &log_path,
            format!(
                "packages.json5 not found or empty at {}; run `peppy repo refresh` to populate it",
                cache::cache_path(&peppy_dirs).display()
            ),
        );
    }

    let resolution = match resolve_transitive_closure(
        &peppy_dirs,
        &entries,
        &root_name,
        &root_tag,
        &dep_variant_overrides,
        &action_context,
        &feedback_tx,
    )
    .await
    {
        Ok(r) => r,
        Err(msg) => return fail(&log_file, &log_path, msg),
    };

    // Warn about overrides that targeted nodes not actually in the tree.
    for ov in &dep_variant_overrides {
        let in_tree = resolution
            .to_add
            .iter()
            .any(|n| n.name == ov.name && n.tag == ov.tag)
            || resolution
                .stack_skipped
                .iter()
                .any(|(n, t)| n == &ov.name && t == &ov.tag);
        if !in_tree {
            emit(
                &feedback_tx,
                FeedbackStream::Warning,
                format!(
                    "Dependency variant override for {}:{} ignored — not in the resolved dependency tree",
                    ov.name, ov.tag
                ),
            );
        }
    }

    // Warn about overrides clashing with already-in-stack deps.
    for (n, t) in &resolution.stack_skipped {
        if let Some(ov) = dep_variant_overrides
            .iter()
            .find(|o| &o.name == n && &o.tag == t)
        {
            emit(
                &feedback_tx,
                FeedbackStream::Warning,
                format!(
                    "Dependency variant override for {}:{} ignored — node already in stack (requested variant: {})",
                    ov.name, ov.tag, ov.variant
                ),
            );
        }
    }

    if resolution.to_add.is_empty() {
        return fail(
            &log_file,
            &log_path,
            format!(
                "{}:{} has no materializable nodes — it may already be in the stack and have no new deps",
                root_name, root_tag
            ),
        );
    }

    let tree_input: Vec<(PathBuf, config::node::NodeConfig)> = resolution
        .to_add
        .iter()
        .map(|n| (n.root_dir.clone(), n.config_resolved.clone()))
        .collect();
    let tree = match VirtualDeptree::build(tree_input) {
        Ok(t) => t,
        Err(e) => {
            return fail(
                &log_file,
                &log_path,
                format!("Dependency resolution failed: {}", e),
            );
        }
    };

    // Build a (name, tag) -> ResolvedBatchNode lookup for variant info.
    let node_lookup: HashMap<(String, String), &ResolvedBatchNode> = resolution
        .to_add
        .iter()
        .map(|n| ((n.name.clone(), n.tag.clone()), n))
        .collect();

    emit(
        &feedback_tx,
        FeedbackStream::Stdout,
        format!(
            "Batch resolved — {} node(s) to add ({} already in stack): {}",
            resolution.to_add.len(),
            resolution.stack_skipped.len(),
            resolution
                .to_add
                .iter()
                .map(|n| format!("{}:{}", n.name, n.tag))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );

    let mut rollback = RollbackGuard::new(Arc::clone(&action_context.node_stack));

    let mut last_sub_log_path: Option<PathBuf> = None;
    for info in tree.topological_order() {
        let key = info.key();
        let Some(node) = node_lookup.get(&key) else {
            return fail(
                &log_file,
                &log_path,
                format!(
                    "internal error: topological order produced a node not in the batch ({}:{})",
                    key.0, key.1
                ),
            );
        };

        emit(
            &feedback_tx,
            FeedbackStream::Stdout,
            format!(
                "Adding {}:{} ({})",
                node.name,
                node.tag,
                kind_label(node.source_kind)
            ),
        );

        let sub_result =
            match run_single_batched_add(node, &goal, &action_context, &feedback_tx).await {
                Ok(r) => r,
                Err(msg) => {
                    return fail(
                        &log_file,
                        &log_path,
                        format!("Failed to add {}:{}: {}", node.name, node.tag, msg),
                    );
                }
            };

        if !sub_result.success {
            let msg = sub_result
                .error_message
                .unwrap_or_else(|| "unknown node-add failure".to_owned());
            return fail(
                &log_file,
                &log_path,
                format!("Failed to add {}:{}: {}", node.name, node.tag, msg),
            );
        }

        last_sub_log_path = Some(sub_result.log_path.clone());
        rollback.added.push((node.name.clone(), node.tag.clone()));
    }

    // Batch succeeded — defuse rollback and report.
    rollback.disarm();

    emit(
        &feedback_tx,
        FeedbackStream::Stdout,
        format!(
            "Batch add complete — {} node(s) added",
            resolution.to_add.len()
        ),
    );

    let effective_log = last_sub_log_path.unwrap_or(log_path);
    NodeAddResult::success(effective_log, root_name, root_tag)
}

fn kind_label(kind: RepoSourceKind) -> &'static str {
    match kind {
        RepoSourceKind::Fs => "fs",
        RepoSourceKind::Git => "git",
        RepoSourceKind::Url => "http",
    }
}

/// One node that's been materialized on disk and is ready to be fed to
/// the per-node add pipeline.
struct ResolvedBatchNode {
    name: String,
    tag: String,
    root_dir: PathBuf,
    config_resolved: config::node::NodeConfig,
    source_kind: RepoSourceKind,
    /// Caller-requested variant for this node, if any.
    variant_override: Option<String>,
    /// Only set to `true` for the root of the batch. Controls whether
    /// the env_vars / force / root variant from the original goal apply.
    is_root: bool,
}

struct Resolution {
    /// Every node we need to push onto the stack, in discovery order.
    /// Topological order is applied later via VirtualDeptree.
    to_add: Vec<ResolvedBatchNode>,
    /// Deps that were already in the stack when we started — we skip
    /// these and rely on the existing entity.
    stack_skipped: Vec<(String, String)>,
}

type MaterializeOutput = (
    String,
    String,
    bool,
    RepoSourceKind,
    Result<(PathBuf, ParsedNodeConfig), String>,
);

async fn resolve_transitive_closure<'a>(
    peppy_dirs: &'a PeppyDirs,
    entries: &'a [PackageEntry],
    root_name: &str,
    root_tag: &str,
    dep_overrides: &[DepVariantOverride],
    action_context: &NodeAddActionContext,
    feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
) -> Result<Resolution, String> {
    let override_map: HashMap<(String, String), String> = dep_overrides
        .iter()
        .map(|o| ((o.name.clone(), o.tag.clone()), o.variant.clone()))
        .collect();

    let mut to_add: Vec<ResolvedBatchNode> = Vec::new();
    let mut stack_skipped: Vec<(String, String)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut pending: Vec<(String, String, bool)> =
        vec![(root_name.to_owned(), root_tag.to_owned(), true)];
    let mut in_flight: FuturesUnordered<BoxFuture<'a, MaterializeOutput>> = FuturesUnordered::new();

    loop {
        while let Some((name, tag, is_root)) = pending.pop() {
            let key = (name.clone(), tag.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            // Deps already in the node stack → skip (but only for non-root;
            // the root is the user's explicit target — we add/replace it).
            if !is_root && action_context.node_stack.find(&name, &tag).is_some() {
                stack_skipped.push(key);
                continue;
            }
            let Some(entry) = cache::lookup(entries, &name, &tag) else {
                missing.push(key);
                continue;
            };
            let entry = entry.clone();
            let source_kind = entry.source_type;
            in_flight.push(Box::pin(async move {
                let result = materialize_entry(&entry, peppy_dirs, feedback_tx).await;
                (name, tag, is_root, source_kind, result)
            }));
        }

        let Some((name, tag, is_root, source_kind, result)) = in_flight.next().await else {
            break;
        };

        let (root_dir, parsed) = match result {
            Ok(pair) => pair,
            Err(e) => {
                return Err(format!(
                    "Failed to materialize {}:{} from repo cache: {}",
                    name, tag, e
                ));
            }
        };

        // Roots take their variant from goal.variant (handled later in
        // run_single_batched_add); deps look at override_map.
        let variant_override = if is_root {
            None
        } else {
            override_map.get(&(name.clone(), tag.clone())).cloned()
        };

        // Enforce that an override points at a variant declared by this
        // dep's manifest. (Root variant validation happens inside
        // run_node_add itself.)
        if let Some(ref v) = variant_override
            && !parsed.variant_names().iter().any(|n| n == v)
        {
            return Err(format!(
                "variant '{v}' not declared on dep {name}:{tag} (available: {:?})",
                parsed.variant_names()
            ));
        }

        if let Some(deps) = parsed.manifest().depends_on.as_ref() {
            for dep in &deps.nodes {
                let dep_name = dep.name.as_str().to_owned();
                let dep_tag = dep.tag.clone();
                if seen.contains(&(dep_name.clone(), dep_tag.clone())) {
                    continue;
                }
                pending.push((dep_name, dep_tag, false));
            }
        }

        // VirtualDeptree only reads `manifest.depends_on` from this config
        // for the topological sort; execution is supplied by `run_node_add`
        // later from the working dir. For variant-only nodes (no root-level
        // execution) `into_resolved()` rightfully refuses, so we use the
        // display-friendly fallback.
        let config_resolved = parsed.clone().into_resolved_or_default();

        to_add.push(ResolvedBatchNode {
            name,
            tag,
            root_dir,
            config_resolved,
            source_kind,
            variant_override,
            is_root,
        });
    }

    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|(n, t)| format!("{}:{}", n, t))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Dependencies missing from packages cache: {list}. Run `peppy repo refresh` or add the missing nodes to a configured repository."
        ));
    }

    Ok(Resolution {
        to_add,
        stack_skipped,
    })
}

/// Materialize one package cache entry to a `(root_dir, parsed config)`
/// pair, using the persistent git/http caches where applicable.
async fn materialize_entry(
    entry: &PackageEntry,
    peppy_dirs: &PeppyDirs,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
) -> Result<(PathBuf, ParsedNodeConfig), String> {
    let root_dir = match entry.source_type {
        RepoSourceKind::Fs => PathBuf::from(&entry.path),
        RepoSourceKind::Git => {
            let url = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| "Git cache entry missing source_uri".to_owned())?;
            let reference = entry.resolved_ref.as_deref();
            let fb = feedback_tx.clone();
            let peppy_dirs = peppy_dirs.clone();
            let url_owned = url.to_owned();
            let ref_owned = reference.map(|s| s.to_owned());
            let checkout = tokio::task::spawn_blocking(move || {
                node_cache::ensure_checkout(
                    &peppy_dirs,
                    &url_owned,
                    ref_owned.as_deref(),
                    &|line| {
                        let _ = fb.send(FeedbackLine {
                            stream: FeedbackStream::Stdout,
                            line: line.to_owned(),
                        });
                    },
                )
            })
            .await
            .map_err(|e| format!("git cache task failed: {}", e))??;
            checkout.join(&entry.path)
        }
        RepoSourceKind::Url => {
            let url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| "Http cache entry missing source_uri".to_owned())?;
            let url = Url::parse(url_str)
                .map_err(|e| format!("Http cache entry has invalid URL '{url_str}': {e}"))?;
            let fb = feedback_tx.clone();
            node_cache::ensure_bundle(peppy_dirs, &url, None, &move |line| {
                let _ = fb.send(FeedbackLine {
                    stream: FeedbackStream::Stdout,
                    line: line.to_owned(),
                });
            })
            .await?
        }
    };

    let config_path = root_dir.join(NODE_CONFIG_FILE);
    let parsed = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;
    Ok((root_dir, parsed))
}

/// Run a single node-add step inside the batch. Each sub-add gets its
/// own log file; the returned `NodeAddResult.log_path` is propagated up
/// so the user can still find the most recent log.
async fn run_single_batched_add(
    node: &ResolvedBatchNode,
    batch_goal: &NodeAddGoal,
    action_context: &NodeAddActionContext,
    batch_feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
) -> Result<NodeAddResult, String> {
    let mut sub_goal = NodeAddGoal::new(
        node.root_dir.clone(),
        STACK_LAUNCH_GIT_HASH,
        batch_goal.timeout_secs,
    );

    if node.is_root {
        // Env vars / force / root variant only apply to the root entity.
        sub_goal = sub_goal
            .with_env_vars(batch_goal.env_vars.clone())
            .with_force(batch_goal.force);
        if let Some(ref v) = batch_goal.variant {
            sub_goal = sub_goal.with_variant_source(v.clone());
        }
    } else if let Some(ref v) = node.variant_override {
        sub_goal = sub_goal.with_variant_name(v.clone());
    }

    // Each sub-add gets its own log file derived from `{name}_{tag}`.
    let log_dir = action_context.peppy_dirs.logs_dir_add();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}_{}.log", node.name, node.tag, timestamp);
    let (log_file, log_path) = create_action_log_file(&log_dir, &log_filename)
        .map_err(|e| format!("Failed to create sub-add log: {}", e))?;

    // Mirror sub-add feedback onto the batch feedback channel so users see a
    // single stream of progress for the whole batch.
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let batch_fb = batch_feedback_tx.clone();
    let forwarder = tokio::spawn(async move {
        while let Some(line) = sub_rx.recv().await {
            let _ = batch_fb.send(line);
        }
    });

    let action_context = action_context.clone();
    let result = run_node_add(
        sub_goal,
        action_context,
        sub_tx,
        log_file,
        log_path,
        timestamp,
    )
    .await;
    let _ = forwarder.await;
    Ok(result)
}

/// Drop-based rollback: if the guard is dropped armed, every
/// `(name, tag)` in `added` is removed from the node stack in reverse
/// order (so dependants go first, then deps). On success, call
/// [`RollbackGuard::disarm`].
struct RollbackGuard {
    node_stack: Arc<node_stack::NodeStack>,
    added: Vec<(String, String)>,
    armed: bool,
}

impl RollbackGuard {
    fn new(node_stack: Arc<node_stack::NodeStack>) -> Self {
        Self {
            node_stack,
            added: Vec::new(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RollbackGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for (name, tag) in self.added.drain(..).rev() {
            if let Err(e) = self.node_stack.remove_config(&name, &tag) {
                warn!("Batch-add rollback failed for {}:{} — {}", name, tag, e);
            } else {
                debug!("Rolled back batched add of {}:{}", name, tag);
            }
        }
    }
}

fn fail(log_file: &Arc<StdMutex<File>>, log_path: &std::path::Path, msg: String) -> NodeAddResult {
    super::write_error_to_log(log_file, &msg);
    NodeAddResult::failure(log_path, msg)
}

fn emit(feedback_tx: &mpsc::UnboundedSender<FeedbackLine>, stream: FeedbackStream, line: String) {
    let _ = feedback_tx.send(FeedbackLine { stream, line });
}
