//! Batch-add pipeline for `NodeSource::RepoNode` goals.
//!
//! Resolves a `(name, tag)` target against `~/.peppy/cache/nodes.json5`,
//! walks its transitive deps, materializes every node through the
//! persistent git/http caches, topologically sorts them via
//! [`VirtualDeptree`], then feeds each to [`super::add::run_node_add`].
//! Mid-batch failure rolls back every node pushed during the batch via a
//! drop guard.

use super::super::repo::cache::{self, NodeCacheEntry};
use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::add::{NodeAddActionContext, run_node_add};
use super::cache as node_cache;
use super::{FeedbackLine, FeedbackStream, create_action_log_file};
use chrono::Local;
use config::consts::PeppyDirs;
use config::node::ParsedNodeConfig;
use core_node_api::encoding::{NodeAddGoal, NodeAddResult, NodeSource, RepoSourceKind};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use node_stack::VirtualDeptree;
use parking_lot::Mutex as StdMutex;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, warn};

/// Upper bound on concurrently-running `materialize_entry` tasks inside a
/// single batch. Bundles are materialized through git clones and HTTP
/// downloads; spawning an unbounded number of them at once thrashes disk
/// and network. 8 is empirical — enough to overlap IO latency, low
/// enough to avoid saturating a developer laptop.
const MATERIALIZE_CONCURRENCY: usize = 8;

/// Entry point called from `handle_goal_request` when
/// `goal.source` is `NodeSource::RepoNode`.
pub(crate) async fn run_repo_node_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeAddResult {
    let (root_name, root_tag) = match &goal.source {
        NodeSource::RepoNode { name, tag } => (name.clone(), tag.clone()),
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

    let (entries, cache_generation) = match cache::load_with_generation(&peppy_dirs) {
        Ok(loaded) => loaded,
        Err(e) => {
            return fail(
                &log_file,
                &log_path,
                format!("Failed to read nodes cache: {}", e),
            );
        }
    };
    if entries.is_empty() {
        return fail(
            &log_file,
            &log_path,
            format!(
                "nodes.json5 not found or empty at {}; run `peppy repo refresh` to populate it",
                cache::nodes_repo_cache_path(&peppy_dirs).display()
            ),
        );
    }

    let resolution_ctx = BatchResolutionCtx {
        peppy_dirs: &peppy_dirs,
        entries: &entries,
        cache_generation,
        feedback_tx: &feedback_tx,
    };
    let resolution = match resolve_transitive_closure(resolution_ctx, &root_name, &root_tag).await {
        Ok(r) => r,
        Err(msg) => return fail(&log_file, &log_path, msg),
    };

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
            "Batch resolved — {} node(s) to add: {}",
            resolution.to_add.len(),
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
        let Some(node) = node_lookup.get(&(key.name.clone(), key.tag.clone())) else {
            return fail(
                &log_file,
                &log_path,
                format!(
                    "internal error: topological order produced a node not in the batch ({})",
                    key.label()
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

        // Snapshot any pre-existing config before the sub-add replaces it
        // in place. On rollback we re-install this config instead of
        // removing the slot, so an in-place replacement that later fails
        // elsewhere in the batch does not wipe the user's prior state.
        let node_variant = node
            .variant_override
            .as_deref()
            .unwrap_or(node_stack::DEFAULT_VARIANT);
        let previous = action_context
            .node_stack
            .find(&node.name, &node.tag, node_variant)
            .map(|handle| {
                let guard = handle.read();
                PreviousConfig {
                    config: guard.config().clone(),
                    config_path: guard.config_path().to_path_buf(),
                    variant_name: guard.variant_name().to_owned(),
                }
            });

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
        rollback.added.push(RollbackEntry {
            name: node.name.clone(),
            tag: node.tag.clone(),
            variant: node_variant.to_owned(),
            previous,
        });
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
    let root_variant = resolution
        .to_add
        .iter()
        .find(|n| n.is_root)
        .and_then(|n| n.variant_override.clone())
        .unwrap_or_else(|| node_stack::DEFAULT_VARIANT.to_owned());
    NodeAddResult::success(effective_log, root_name, root_tag, root_variant)
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
}

type MaterializeOutput = (
    String,
    String,
    bool,
    RepoSourceKind,
    Result<(PathBuf, ParsedNodeConfig), String>,
);

/// Bundles the cache/IO dependencies threaded through the batch-resolution
/// pipeline so callers don't juggle half a dozen parallel borrows.
#[derive(Clone, Copy)]
struct BatchResolutionCtx<'a> {
    peppy_dirs: &'a PeppyDirs,
    entries: &'a [NodeCacheEntry],
    cache_generation: Option<SystemTime>,
    feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
}

async fn resolve_transitive_closure<'a>(
    ctx: BatchResolutionCtx<'a>,
    root_name: &str,
    root_tag: &str,
) -> Result<Resolution, String> {
    let BatchResolutionCtx {
        peppy_dirs,
        entries,
        cache_generation,
        feedback_tx,
    } = ctx;

    let mut to_add: Vec<ResolvedBatchNode> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut pending: Vec<(String, String, bool)> =
        vec![(root_name.to_owned(), root_tag.to_owned(), true)];
    let mut in_flight: FuturesUnordered<BoxFuture<'a, MaterializeOutput>> = FuturesUnordered::new();
    let semaphore = Arc::new(Semaphore::new(MATERIALIZE_CONCURRENCY));

    loop {
        while let Some((name, tag, is_root)) = pending.pop() {
            let key = (name.clone(), tag.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            // Every resolved node (root and deps alike) is materialized and
            // pushed. `push_config_impl` handles in-place replacement for
            // keys already in the stack, including the live-instance and
            // dependents safety gates.
            let Some(entry) = cache::lookup(entries, &name, &tag) else {
                missing.push(key);
                continue;
            };
            let entry = entry.clone();
            let source_kind = entry.source_type;
            let permit_source = Arc::clone(&semaphore);
            let fb = feedback_tx.clone();
            let on_feedback: node_cache::MaterializeFeedback = Arc::new(move |line: &str| {
                let _ = fb.send(FeedbackLine {
                    stream: FeedbackStream::Stdout,
                    line: line.to_owned(),
                });
            });
            in_flight.push(Box::pin(async move {
                let _permit = permit_source
                    .acquire_owned()
                    .await
                    .expect("materialize semaphore is never closed");
                let result = node_cache::materialize_entry(
                    &entry,
                    peppy_dirs,
                    cache_generation,
                    on_feedback,
                )
                .await;
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
        // run_single_batched_add); deps inherit the default variant and
        // are explicitly added at the desired variant via a separate
        // `peppy node add` invocation when a non-default is required.
        let variant_override: Option<String> = None;

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
            "Dependencies missing from nodes cache ({}): {list}. Run `peppy repo refresh` or add the missing nodes to a configured repository.",
            cache::nodes_repo_cache_path(peppy_dirs).display()
        ));
    }

    Ok(Resolution { to_add })
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

    // Each sub-add gets its own log file derived from
    // `{name}_{tag}__{variant}`.
    let log_dir = action_context.peppy_dirs.logs_dir_add();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_variant = node
        .variant_override
        .as_deref()
        .unwrap_or(node_stack::DEFAULT_VARIANT);
    let log_filename = format!(
        "{}_{}__{}_{}.log",
        node.name, node.tag, log_variant, timestamp
    );
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

/// Snapshot of a stack entity captured before the batch replaced it, so
/// rollback can re-install the prior config instead of removing the slot.
/// Artifact/stage state is intentionally omitted: a successful rollback
/// returns the entity to `Added` (pending build); any previously built
/// artifact remains on disk and a follow-up `node build` rewires it.
struct PreviousConfig {
    config: config::node::NodeConfig,
    config_path: PathBuf,
    variant_name: String,
}

struct RollbackEntry {
    name: String,
    tag: String,
    variant: String,
    previous: Option<PreviousConfig>,
}

/// Drop-based rollback: if the guard is dropped armed, every entry in
/// `added` is undone in reverse order (so dependants go first, then
/// deps). Entries that replaced an existing config restore the previous
/// state via `push_config_with_variant`; entries that introduced a new
/// slot are removed. On success, call [`RollbackGuard::disarm`].
struct RollbackGuard {
    node_stack: Arc<node_stack::NodeStack>,
    added: Vec<RollbackEntry>,
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
        for entry in self.added.drain(..).rev() {
            let RollbackEntry {
                name,
                tag,
                variant,
                previous,
            } = entry;
            match previous {
                Some(prev) => match self.node_stack.push_config_with_variant(
                    prev.config,
                    false,
                    prev.config_path,
                    prev.variant_name,
                ) {
                    Ok(()) => debug!(
                        "Rolled back batched replacement of {}:{}@{}",
                        name, tag, variant
                    ),
                    Err(e) => warn!(
                        "Batch-add rollback (restore previous) failed for {}:{}@{}: {}",
                        name, tag, variant, e
                    ),
                },
                None => match self.node_stack.remove_config(&name, &tag, &variant) {
                    Ok(_) => debug!("Rolled back batched add of {}:{}@{}", name, tag, variant),
                    Err(e) => warn!(
                        "Batch-add rollback failed for {}:{}@{}: {}",
                        name, tag, variant, e
                    ),
                },
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
