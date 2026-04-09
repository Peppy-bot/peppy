use parking_lot::RwLock;
use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use config::consts::PeppyDirs;
use config::node::{Name, NodeConfig};
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks, spawn_output_reader_async};
use crate::error::{Error, Result};

use super::build_steps::{
    ContainerBuildInputs, archive_dir_to_storage, build_container_image, move_sif_to_storage,
    run_add_cmd,
};
use super::start_steps::{
    create_instance_dir, extract_node_archive, kill_and_collect_error, spawn_container_node,
    spawn_process_node,
};

/// Serializable representation of a node in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNode {
    pub name: String,
    pub tag: String,
    /// Path to the `peppy.json5` config that registered this node.
    /// Always present (set when the entity reaches `Added`).
    pub config_path: String,
    /// Path to the built `.sif`/archive in `~/.peppy/added_nodes`.
    /// `None` until the entity reaches `Built`.
    pub artifact_path: Option<String>,
    /// IDs of running instances. Empty unless the entity is `Started`.
    pub instance_ids: Vec<String>,
}

impl SerializedNode {
    /// Returns a display label in the format "name:tag".
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }

    /// Returns the number of instances.
    pub fn instance_count(&self) -> usize {
        self.instance_ids.len()
    }

    /// Returns instance info in the format "N instance(s): ["id1", "id2"]".
    pub fn instance_info(&self) -> String {
        let count = self.instance_count();
        let suffix = if count == 1 { "instance" } else { "instances" };
        let ids: Vec<String> = self
            .instance_ids
            .iter()
            .map(|id| format!("\"{}\"", id))
            .collect();
        format!("{} {}: [{}]", count, suffix, ids.join(", "))
    }
}

/// Serializable representation of a dependency edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub from: SerializedNode,
    pub to: SerializedNode,
}

/// Serializable representation of the entire node graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNodeGraph {
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}

impl From<&NodeEntity> for SerializedNode {
    fn from(entity: &NodeEntity) -> Self {
        Self {
            name: entity.config().manifest.name.as_str().to_string(),
            tag: entity.config().manifest.tag.clone(),
            config_path: entity.config_path().display().to_string(),
            artifact_path: entity.artifact_path().map(|p| p.display().to_string()),
            // Externally visible instance ids — only `Running` instances.
            // In-flight `Starting` instances are intentionally hidden from
            // CLI/dashboard consumers because the externally-visible meaning
            // of "instance_ids" is "what's currently running and reachable".
            // Exposing Starting instances here would let external observers
            // try to interact with something that hasn't subscribed to
            // messenger services yet.
            instance_ids: entity
                .instances()
                .iter()
                .filter(|i| i.state() == InstanceState::Running)
                .map(|i| i.instance_id().as_str().to_string())
                .collect(),
        }
    }
}

/// Lifecycle stage of a `NodeEntity`. Describes *artifact readiness only* —
/// per-instance state (Starting/Running) lives on each
/// [`TrackedNodeInstance`] inside `Ready.instances`.
///
/// - `Added` — config registered, no artifact, no instances.
/// - `Building` — `build()` is running its I/O. Acts as the concurrency
///   barrier: a second concurrent `build()` on the same entity sees this
///   stage and is rejected immediately with no queueing.
/// - `Ready` — artifact is on disk. The instances list may be empty (no
///   instances spawned yet, equivalent to the old `Built` stage), contain
///   only `Running` instances (equivalent to the old `Started` stage), or
///   contain a mix of `Starting` and `Running` instances (in-flight
///   `prepare_and_spawn` calls coexisting with already-running instances).
/// - `Root` — the synthetic daemon entity. Has no buildable artifact and
///   exactly one `Running` instance (the daemon process itself). The
///   lifecycle methods (`build`, `prepare_and_spawn`, `commit_started`,
///   `stop_instance`, …) all reject this variant: the root cannot be
///   built, started, stopped, or removed. It exists so the daemon can
///   appear in the same `NodeStack` graph as user nodes.
#[derive(Debug, Clone)]
pub enum NodeStage {
    Added {
        config_path: PathBuf,
    },
    Building {
        config_path: PathBuf,
    },
    Ready {
        config_path: PathBuf,
        artifact_path: PathBuf,
        instances: Vec<TrackedNodeInstance>,
    },
    // Special kind
    Root {
        config_path: PathBuf,
        instance: TrackedNodeInstance,
    },
}

impl NodeStage {
    pub fn name(&self) -> &'static str {
        match self {
            NodeStage::Added { .. } => "Added",
            NodeStage::Building { .. } => "Building",
            NodeStage::Ready { .. } => "Ready",
            NodeStage::Root { .. } => "Root",
        }
    }

    /// Pure validator: returns `Ok(())` if `NodeEntity::build` is allowed
    /// from this stage (only `Added` is), or `Err(current_stage_name)`
    /// otherwise. The production `build()` method calls this before its
    /// own field-extracting match, so the rejection rule lives in exactly
    /// one place and the parametric rejection tests can exercise it
    /// without needing to inject stages into a real entity.
    pub fn ensure_buildable(&self) -> std::result::Result<(), &'static str> {
        match self {
            NodeStage::Added { .. } => Ok(()),
            other => Err(other.name()),
        }
    }

    /// Pure validator: returns `Ok(())` if `NodeEntity::prepare_and_spawn`
    /// is allowed from this stage (only `Ready` is), or
    /// `Err(current_stage_name)` otherwise. Mirrors the structural shape
    /// of [`NodeStage::ensure_buildable`] — see its doc for the rationale.
    pub fn ensure_spawnable(&self) -> std::result::Result<(), &'static str> {
        match self {
            NodeStage::Ready { .. } => Ok(()),
            other => Err(other.name()),
        }
    }
}

/// Inputs required to drive [`NodeEntity::build`] to completion.
///
/// The struct holds borrows so callers don't have to clone heavy state. The
/// caller is responsible for keeping the working directory alive for the
/// duration of the build.
pub struct BuildContext<'a> {
    /// Temporary working directory containing the node sources and (for
    /// container nodes) the apptainer `.def` file. For process nodes, the
    /// user-defined `add_cmd` is executed inside this directory before
    /// archiving. The build artifact is produced inside this directory and
    /// then moved to peppy storage.
    pub working_dir: &'a Path,
    /// Resolved peppy directory layout. The built `.sif`/archive is placed
    /// inside `peppy_dirs.added_nodes_dir()`.
    pub peppy_dirs: &'a PeppyDirs,
    /// Channel that streams stdout/stderr lines from the build child process
    /// (and from `add_cmd`, for process nodes) back to the caller.
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    /// Log file the build output is also written to.
    pub log_file: Arc<StdMutex<File>>,
    /// Environment variables passed to `add_cmd` (process nodes only). The
    /// daemon prepares this list via `validate_goal_env_vars`,
    /// `inject_rust_build_env`, and `inject_node_runtime_env`. Container
    /// nodes ignore this field — apptainer build does not consume it.
    pub env_vars: &'a [(String, String)],
}

/// Inputs required to drive [`NodeEntity::prepare_and_spawn`] to completion.
///
/// Mirrors [`BuildContext`] for the start path. The struct holds borrows so
/// callers don't have to clone heavy state. Messenger-bound parameters
/// (`signal_target`, ready/health checks) live in the daemon, not here.
pub struct StartContext<'a> {
    /// Instance identifier — used for log messages, runtime-config file
    /// naming, and the eventual `TrackedNodeInstance::new`.
    pub instance_id: &'a Name,
    /// The runtime config to write to the per-spawn temp file. For container
    /// nodes, the caller is responsible for any host_gateway rewriting before
    /// calling start (the entity treats this as opaque bytes).
    pub runtime_config_json5: &'a str,
    /// User + injected env vars (already passed through
    /// `validate_goal_env_vars`, `inject_rust_build_env`, and
    /// `inject_node_runtime_env` in core-node).
    pub env_vars: &'a [(String, String)],
    /// Mount paths with `${parameters:...}` already resolved by core-node
    /// (against runtime arguments and the blocked-source policy). Container
    /// nodes only.
    pub mount_paths_resolved: &'a [String],
    /// Resolved peppy directory layout. Used for `runtime_config_dir()` and
    /// `instances_dir()`.
    pub peppy_dirs: &'a PeppyDirs,
    /// Output-pipeline plumbing. The entity does not inspect these fields —
    /// it forwards them verbatim into `spawn_output_reader_async`.
    pub output_sinks: OutputSinks,
}

/// Output-pipeline plumbing forwarded by [`NodeEntity::prepare_and_spawn`]
/// into the spawned reader tasks. Grouped into its own struct so the
/// entity's `StartContext` surface only carries fields the entity actually
/// reasons about.
pub struct OutputSinks {
    /// Channel that receives stdout/stderr lines from the running child. The
    /// reader tasks remain alive past `prepare_and_spawn`'s return.
    pub feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    /// Log file the start output is also written to.
    pub log_file: Arc<StdMutex<File>>,
    /// Gate that the daemon flips to `false` after `commit_started` /
    /// `abort_started` returns, so that further reader-task lines stop being
    /// forwarded onto the daemon's external feedback topic. The reader tasks
    /// themselves stay alive — only their forwarding is disabled.
    pub publish_enabled: Arc<AtomicBool>,
    /// Hooks invoked by the output reader on first stdout line and on each
    /// successful publish. The daemon implements this for its `FeedbackSync`
    /// quiescence-detection primitive; tests can pass a no-op.
    pub hooks: Arc<dyn OutputReaderHooks>,
}

/// State handed back from [`NodeEntity::prepare_and_spawn`]. The caller owns
/// it across `.await` boundaries (so it must be `Send`) and passes it to
/// either [`NodeEntity::commit_started`] or [`NodeEntity::abort_started`].
#[derive(Debug)]
pub struct StartedInstanceCtx {
    pub instance_dir: PathBuf,
    pub runtime_config_path: PathBuf,
    pub stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
    pub output_reader_handles: Vec<JoinHandle<std::io::Result<()>>>,
    pub log_file: Arc<StdMutex<File>>,
    /// Snapshot of the entity's `generation` taken at `prepare_and_spawn`
    /// time. `commit_started`/`abort_started` compare this against the
    /// current entity generation and refuse to mutate the replacement entity
    /// if a concurrent `push_config` has bumped the generation in the
    /// meantime — they only clean up the stale child/context.
    pub generation: u64,
}

/// Process-wide monotonic counter that assigns each `NodeEntity` instance a
/// unique generation. The build path snapshots this when transitioning into
/// `Building`, and rejects the publish if the entity it observes after I/O
/// has a different generation — i.e. a concurrent `push_config` replaced the
/// entity contents in the meantime.
static NEXT_ENTITY_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_entity_generation() -> u64 {
    NEXT_ENTITY_GENERATION.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug)]
pub struct NodeEntity {
    config: NodeConfig,
    stage: NodeStage,
    /// Unique-per-construction token used to detect when an in-flight `build`
    /// is racing against a wholesale entity replacement (`push_config_impl`'s
    /// in-place overwrite). Bumped on every `new`, `from_snapshot`, and
    /// `root` construction. The build path captures this value at Phase 1 and
    /// re-checks it at Phase 3 before publishing artifacts.
    generation: u64,
    /// Path to the most-recent add/build log file produced for this entity.
    /// Set by the add/launch services after `create_action_log_file`
    /// succeeds. Reset to `None` whenever the entity is replaced wholesale
    /// (a fresh `NodeEntity::new` is constructed for the same key by
    /// `push_config_impl`). `None` for the synthetic root entity, which
    /// has no add log.
    last_add_log_path: Option<PathBuf>,
}

impl NodeEntity {
    /// Creates a new `NodeEntity` in the [`NodeStage::Added`] stage. The
    /// `config_path` should point at the `peppy.json5` file that supplied
    /// `config`.
    pub fn new<P: Into<PathBuf>>(config: NodeConfig, config_path: P) -> Self {
        Self {
            config,
            stage: NodeStage::Added {
                config_path: config_path.into(),
            },
            generation: next_entity_generation(),
            last_add_log_path: None,
        }
    }

    /// Returns the path to the most-recent add/build log produced for this
    /// entity, if any. See [`NodeEntity::set_last_add_log_path`].
    pub fn last_add_log_path(&self) -> Option<&Path> {
        self.last_add_log_path.as_deref()
    }

    /// Records the path of the add/build log file the daemon just opened
    /// for this entity. Called by the add/launch services right after
    /// `create_action_log_file` succeeds, under the same write lock as the
    /// rest of the entity transition.
    pub fn set_last_add_log_path(&mut self, path: PathBuf) {
        self.last_add_log_path = Some(path);
    }

    /// Returns the entity's monotonic generation token. Bumped on every
    /// `new`/`from_snapshot`/`root` construction so the build path can
    /// distinguish "still the entity I started building" from "wholesale
    /// replaced by a concurrent push_config".
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn stage(&self) -> &NodeStage {
        &self.stage
    }

    /// Returns the `peppy.json5` path that registered this entity. Always
    /// present regardless of which stage the entity is in.
    pub fn config_path(&self) -> &Path {
        match &self.stage {
            NodeStage::Added { config_path } => config_path,
            NodeStage::Building { config_path } => config_path,
            NodeStage::Ready { config_path, .. } => config_path,
            NodeStage::Root { config_path, .. } => config_path,
        }
    }

    /// Returns the path to the built `.sif`/archive in
    /// `~/.peppy/added_nodes`. `None` until the entity has reached `Ready`,
    /// and `None` for the synthetic root entity (which has no artifact).
    pub fn artifact_path(&self) -> Option<&Path> {
        match &self.stage {
            NodeStage::Added { .. } | NodeStage::Building { .. } | NodeStage::Root { .. } => None,
            NodeStage::Ready { artifact_path, .. } => Some(artifact_path),
        }
    }

    /// Returns the tracked instances of this entity, including both
    /// `Starting` (in-flight) and `Running` (committed) instances. Observers
    /// that only care about fully-started instances should filter by
    /// [`TrackedNodeInstance::state`]. Returns an empty slice for `Added`
    /// and `Building`.
    pub fn instances(&self) -> &[TrackedNodeInstance] {
        match &self.stage {
            NodeStage::Ready { instances, .. } => instances,
            NodeStage::Root { instance, .. } => std::slice::from_ref(instance),
            NodeStage::Added { .. } | NodeStage::Building { .. } => &[],
        }
    }

    /// Performs the actual `.sif`/archive build for the entity behind `handle`
    /// and transitions its stage `Added → Building → Ready` on success.
    ///
    /// For container nodes, this runs `apptainer build` and moves the resulting
    /// `.sif` into `peppy_dirs.added_nodes_dir()`. For process nodes, this
    /// runs the user-defined `add_cmd` (if any) inside `working_dir` and then
    /// archives the working directory into a `.tar.zst` in the same location.
    ///
    /// Concurrency: a second `build` call on the same entity that arrives
    /// while the first is still running observes the `Building` stage and is
    /// rejected immediately with [`Error::InvalidStageTransition`]. There is
    /// no queueing — once an entity is in `Building`, no other lifecycle
    /// transition is allowed until the build resolves.
    ///
    /// Failure contract: on any failure (I/O, storage, archive), the entity
    /// is left in `Building` and is **not** rolled back to `Added`. The caller
    /// owns cleanup — typically by removing the entity from the stack via
    /// `NodeStack::remove_config`. Failed builds are not retryable in place.
    ///
    /// Returns [`Error::InvalidStageTransition`] if the entity is not in
    /// [`NodeStage::Added`], or [`Error::BuildFailed`] if the underlying
    /// `add_cmd` / apptainer / archive step fails.
    /// Drives the entity from `Added` to `Ready`. On success, returns the
    /// `PathBuf` of the artifact freshly installed in storage; the caller
    /// MUST use this returned path rather than re-reading
    /// `entity.artifact_path()` afterwards, because a concurrent
    /// `push_config` could replace the entity in-place between the build
    /// completing and the re-read.
    pub async fn build(handle: &Arc<RwLock<NodeEntity>>, ctx: BuildContext<'_>) -> Result<PathBuf> {
        // ---- Phase 1: Added → Building, snapshot inputs (brief write lock) ----
        let (node_name, node_tag, config_path, container_opt, add_cmd, build_generation) = {
            let mut guard = handle.write();
            if let Err(from) = guard.stage.ensure_buildable() {
                return Err(Error::InvalidStageTransition {
                    node_name: guard.config.manifest.name.as_str().to_owned(),
                    node_tag: guard.config.manifest.tag.clone(),
                    from,
                    to: "Ready",
                });
            }
            let NodeStage::Added { config_path } = &guard.stage else {
                unreachable!("ensure_buildable just verified Added")
            };
            let config_path = config_path.clone();
            let snapshot = (
                guard.config.manifest.name.as_str().to_owned(),
                guard.config.manifest.tag.clone(),
                config_path.clone(),
                guard.config.execution.container.clone(),
                guard.config.execution.add_cmd.clone(),
                guard.generation,
            );
            // Atomic transition Added → Building under the same write lock as
            // the validation. Any second concurrent call now sees Building.
            guard.stage = NodeStage::Building { config_path };
            snapshot
        };

        // ---- Phase 2: I/O without any entity lock ----
        // For container nodes, build the .sif via apptainer.
        // For process nodes, run the user-defined add_cmd (if any).
        // Defer publishing the artifact into shared storage until *after*
        // we re-confirm the entity is still `Building` under the write
        // lock — otherwise a stale build could orphan/overwrite an artifact
        // installed by a competing winner.
        let is_container = container_opt.is_some();
        let io_result: std::result::Result<(), Error> = async {
            if let Some(container) = container_opt {
                let apptainer_build_extra_args = container
                    .apptainer_build_extra_args
                    .as_deref()
                    .unwrap_or_default();
                let lima_shell_extra_args = container
                    .lima_shell_extra_args
                    .as_deref()
                    .unwrap_or_default();

                build_container_image(ContainerBuildInputs {
                    working_dir: ctx.working_dir,
                    node_name: &node_name,
                    node_tag: &node_tag,
                    def_file: &container.def_file,
                    apptainer_build_extra_args,
                    lima_shell_extra_args,
                    feedback_tx: ctx.feedback_tx,
                    log_file: Arc::clone(&ctx.log_file),
                })
                .await
                .map_err(|reason| Error::BuildFailed {
                    node_name: node_name.clone(),
                    node_tag: node_tag.clone(),
                    reason,
                })?;
            } else {
                // Process node: run add_cmd inside the working dir.
                run_add_cmd(
                    add_cmd.as_ref(),
                    ctx.working_dir,
                    ctx.env_vars,
                    ctx.feedback_tx,
                    Arc::clone(&ctx.log_file),
                )
                .await
                .map_err(|reason| Error::BuildFailed {
                    node_name: node_name.clone(),
                    node_tag: node_tag.clone(),
                    reason: format!("add_cmd failed: {}", reason),
                })?;
            }
            Ok(())
        }
        .await;

        // ---- Phase 3: apply transition or rollback (brief write lock) ----
        let mut guard = handle.write();
        // A concurrent `push_config` could have replaced the entity wholesale
        // while we were running I/O. If we are no longer the in-flight build,
        // discard the result rather than clobbering the new state. The
        // wholesale-replacement case is detected via the `generation` token:
        // even if the new entity has been re-pushed AND a fresh `build`
        // started (so the stage is `Building` again), the generation will
        // differ from what we captured in Phase 1.
        if !matches!(guard.stage, NodeStage::Building { .. })
            || guard.generation != build_generation
        {
            return Err(Error::InvalidStageTransition {
                node_name,
                node_tag,
                from: guard.stage.name(),
                to: "Ready",
            });
        }

        match io_result {
            Ok(()) => {
                let artifact_path = if is_container {
                    move_sif_to_storage(ctx.working_dir, &node_name, &node_tag, ctx.peppy_dirs)
                        .map_err(|e| Error::BuildFailed {
                            node_name: node_name.clone(),
                            node_tag: node_tag.clone(),
                            reason: format!("failed to move container image to storage: {}", e),
                        })?
                } else {
                    archive_dir_to_storage(ctx.working_dir, &node_name, &node_tag, ctx.peppy_dirs)
                        .map_err(|e| Error::BuildFailed {
                        node_name: node_name.clone(),
                        node_tag: node_tag.clone(),
                        reason: format!("failed to archive node directory: {}", e),
                    })?
                };

                guard.stage = NodeStage::Ready {
                    config_path,
                    artifact_path: artifact_path.clone(),
                    instances: Vec::new(),
                };
                Ok(artifact_path)
            }
            Err(e) => {
                // Leave the entity in `Building`. The caller owns cleanup
                // (typically `NodeStack::remove_config`). See the doc comment
                // on this method for the failure contract.
                Err(e)
            }
        }
    }

    /// Best-effort removal of an in-flight `Starting` instance. Used by
    /// [`prepare_and_spawn`] (on I/O failure during the spawn/wire phases)
    /// and [`abort_started`] (on caller-side ready/health failure).
    ///
    /// Looks up the instance by id and removes it from the `Ready.instances`
    /// list. Silent no-op if the entity is no longer in `Ready` (concurrent
    /// `push_config` replaced it) or if the instance is missing (already
    /// removed). Also a no-op if the instance is in `Running` state — that
    /// case shouldn't happen in practice (we only remove things we just
    /// inserted as `Starting`), but defensively we don't touch committed
    /// instances.
    fn remove_starting_instance(handle: &Arc<RwLock<NodeEntity>>, instance_id: &Name) {
        let mut guard = handle.write();
        let NodeStage::Ready { instances, .. } = &mut guard.stage else {
            return;
        };
        if let Some(pos) = instances.iter().position(|inst| {
            inst.instance_id() == instance_id && inst.state() == InstanceState::Starting
        }) {
            instances.remove(pos);
        }
    }

    /// Phase 1 of the start lifecycle: validates the entity is in `Ready`,
    /// atomically registers a new `Starting` instance, prepares the instance
    /// directory, spawns the child process, and wires up output streaming.
    /// Returns the spawned `Child` along with a [`StartedInstanceCtx`] that
    /// the caller must hand back to either [`NodeEntity::commit_started`]
    /// (success) or [`NodeEntity::abort_started`] (failure).
    ///
    /// Concurrency: parallel `prepare_and_spawn` calls with **different**
    /// `instance_id`s on the same entity are allowed and run independently.
    /// Each one atomically appends its own `Starting` instance under the
    /// write lock, then runs its I/O without holding the lock. The instances
    /// list is the only shared state and it's updated under the lock.
    ///
    /// Parallel calls with the **same** `instance_id` are rejected via the
    /// duplicate-id check, which happens under the same write lock as the
    /// append (so it is atomic with respect to other parallel callers).
    ///
    /// Returns [`Error::InvalidStageTransition`] if the entity is not in
    /// `Ready` (e.g. still `Added` or currently `Building`), or
    /// [`Error::DuplicateInstanceId`] if `ctx.instance_id` is already tracked.
    ///
    /// On any I/O failure inside this function, the just-registered
    /// `Starting` instance is removed before returning.
    pub async fn prepare_and_spawn(
        handle: &Arc<RwLock<NodeEntity>>,
        ctx: StartContext<'_>,
    ) -> Result<(Child, StartedInstanceCtx)> {
        // ---- Phase 1: register the Starting instance under a brief write lock ----
        let (node_name, node_tag, node_config, artifact_path, start_generation) = {
            let mut guard = handle.write();
            if let Err(from) = guard.stage.ensure_spawnable() {
                return Err(Error::InvalidStageTransition {
                    node_name: guard.config.manifest.name.as_str().to_owned(),
                    node_tag: guard.config.manifest.tag.clone(),
                    from,
                    to: "spawn instance",
                });
            }
            let entity_generation = guard.generation;
            let NodeStage::Ready {
                artifact_path,
                instances,
                ..
            } = &mut guard.stage
            else {
                unreachable!("ensure_spawnable just verified Ready")
            };

            // Reject duplicate instance ids before any I/O. Atomic with the
            // append below — both happen under this write lock.
            if instances
                .iter()
                .any(|inst| inst.instance_id() == ctx.instance_id)
            {
                return Err(Error::DuplicateInstanceId {
                    instance_id: ctx.instance_id.as_str().to_owned(),
                    node_name: guard.config.manifest.name.as_str().to_owned(),
                    node_tag: guard.config.manifest.tag.clone(),
                });
            }

            let snapshot_artifact = artifact_path.clone();
            instances.push(TrackedNodeInstance::new(
                ctx.instance_id.clone(),
                None,
                InstanceState::Starting,
            ));

            (
                guard.config.manifest.name.as_str().to_owned(),
                guard.config.manifest.tag.clone(),
                guard.config.clone(),
                snapshot_artifact,
                entity_generation,
            )
        };

        // ---- Phase 2/3/4: I/O without any entity lock ----
        let instance_id_str = ctx.instance_id.as_str();
        let is_container = node_config.execution.container.is_some();

        // ---- Phase 2: prepare instance dir ----
        let instance_dir = if is_container {
            create_instance_dir(instance_id_str, ctx.peppy_dirs)
        } else {
            extract_node_archive(&artifact_path, instance_id_str, ctx.peppy_dirs)
        }
        .map_err(|reason| {
            Self::remove_starting_instance(handle, ctx.instance_id);
            Error::StartFailed {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                reason,
            }
        })?;

        // ---- Phase 3: spawn the child process ----
        let (mut child, runtime_config_path) =
            if let Some(container) = node_config.execution.container.as_ref() {
                let apptainer_run_extra_args = container
                    .apptainer_run_extra_args
                    .as_deref()
                    .unwrap_or_default();
                let lima_shell_extra_args = container
                    .lima_shell_extra_args
                    .as_deref()
                    .unwrap_or_default();
                spawn_container_node(
                    &artifact_path,
                    &instance_dir,
                    ctx.runtime_config_json5,
                    ctx.env_vars,
                    ctx.mount_paths_resolved,
                    apptainer_run_extra_args,
                    lima_shell_extra_args,
                    &ctx.output_sinks.log_file,
                    &ctx.output_sinks.feedback_tx,
                    ctx.peppy_dirs,
                )
                .await
            } else {
                spawn_process_node(
                    &node_config,
                    &instance_dir,
                    ctx.runtime_config_json5,
                    ctx.env_vars,
                    &ctx.output_sinks.log_file,
                    ctx.peppy_dirs,
                )
            }
            .map_err(|e| {
                // Best-effort cleanup of the instance dir we just materialized.
                // The container/process spawn never started, so nothing else
                // references this directory.
                let _ = std::fs::remove_dir_all(&instance_dir);
                Self::remove_starting_instance(handle, ctx.instance_id);
                Error::StartFailed {
                    node_name: node_name.clone(),
                    node_tag: node_tag.clone(),
                    reason: format!("failed to spawn child: {}", e),
                }
            })?;

        // ---- Phase 4: wire output streaming ----
        let stderr_buffer = Arc::new(StdMutex::new(VecDeque::with_capacity(
            crate::build_io::STDERR_TAIL_LINES,
        )));
        let mut output_reader_handles = Vec::new();

        let sinks = &ctx.output_sinks;
        if let Some(stdout) = child.stdout.take() {
            output_reader_handles.push(spawn_output_reader_async(
                stdout,
                sinks.feedback_tx.clone(),
                Arc::clone(&sinks.publish_enabled),
                Arc::clone(&sinks.hooks),
                FeedbackStream::Stdout,
                None,
                Arc::clone(&sinks.log_file),
            ));
        }

        if let Some(stderr) = child.stderr.take() {
            output_reader_handles.push(spawn_output_reader_async(
                stderr,
                sinks.feedback_tx.clone(),
                Arc::clone(&sinks.publish_enabled),
                Arc::clone(&sinks.hooks),
                FeedbackStream::Stderr,
                Some(Arc::clone(&stderr_buffer)),
                Arc::clone(&sinks.log_file),
            ));
        }

        Ok((
            child,
            StartedInstanceCtx {
                instance_dir,
                runtime_config_path,
                stderr_buffer,
                output_reader_handles,
                log_file: ctx.output_sinks.log_file,
                generation: start_generation,
            },
        ))
    }

    /// Phase 2 (success): records the spawned instance against the entity and
    /// transitions `Starting → Started`. Returns the child's pid.
    ///
    /// Does NOT join the output reader handles — they remain alive past return
    /// so the daemon keeps streaming the running node's stdout/stderr.
    ///
    /// If a concurrent `push_config` replaced the entity wholesale while the
    /// daemon was running its messenger checks, this returns
    /// [`Error::InvalidStageTransition`] **and kills the spawned child** so
    /// no orphan process is left behind. On the success path the `Child` is
    /// dropped without `kill_on_drop`, so the OS process continues running
    /// under its own pid (the daemon manages termination via PID polling in
    /// `stop_instance`).
    pub async fn commit_started(
        handle: &Arc<RwLock<NodeEntity>>,
        mut child: Child,
        started_ctx: StartedInstanceCtx,
        instance_id: Name,
    ) -> Result<u32> {
        let pid = child.id().unwrap_or(0);

        // Helper: on every error path we must kill the still-running child
        // before returning, otherwise we leak an untracked OS process. tokio
        // `Child` does NOT have kill_on_drop set in the spawn helpers.
        async fn kill_child(child: &mut Child) {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // Take the on-disk paths out of `started_ctx` so we can both persist
        // them onto the running instance (success) and clean them up
        // (failure) without partial moves.
        let StartedInstanceCtx {
            instance_dir,
            runtime_config_path,
            generation: start_generation,
            ..
        } = started_ctx;

        let validation_result: Result<()> = {
            let mut guard = handle.write();
            let node_name = guard.config.manifest.name.as_str().to_owned();
            let node_tag = guard.config.manifest.tag.clone();
            // Generation guard: if a concurrent `push_config` replaced the
            // entity wholesale, the new entity has a different generation
            // and is *not* ours to mutate. Fall through to the cleanup
            // branch (kill child + remove temp files) without touching the
            // replacement entity's instances list.
            if guard.generation != start_generation {
                Err(Error::InvalidStageTransition {
                    node_name,
                    node_tag,
                    from: "stale-generation",
                    to: "Running",
                })
            } else {
                match &mut guard.stage {
                    NodeStage::Ready { instances, .. } => {
                        if let Some(inst) = instances
                            .iter_mut()
                            .find(|inst| inst.instance_id() == &instance_id)
                        {
                            if inst.state() != InstanceState::Starting {
                                Err(Error::InvalidStageTransition {
                                    node_name,
                                    node_tag,
                                    from: "Running",
                                    to: "Running",
                                })
                            } else {
                                inst.set_running(
                                    Some(pid),
                                    instance_dir.clone(),
                                    runtime_config_path.clone(),
                                );
                                Ok(())
                            }
                        } else {
                            Err(Error::InvalidStageTransition {
                                node_name,
                                node_tag,
                                from: "missing",
                                to: "Running",
                            })
                        }
                    }
                    other => Err(Error::InvalidStageTransition {
                        node_name,
                        node_tag,
                        from: other.name(),
                        to: "Ready",
                    }),
                }
            }
        };

        match validation_result {
            Ok(()) => {
                // Successful commit: drop the Child without killing. The
                // OS process keeps running and the daemon owns its lifetime.
                drop(child);
                Ok(pid)
            }
            Err(e) => {
                // Concurrent push_config / stale generation / inconsistent
                // state: the entity is no longer ours, but the spawned
                // process and the on-disk artifacts created during this
                // start *are* — kill the child and clean up the temp files
                // before returning so nothing orphans.
                kill_child(&mut child).await;
                let _ = std::fs::remove_dir_all(&instance_dir);
                let _ = std::fs::remove_file(&runtime_config_path);
                Err(e)
            }
        }
    }

    /// Phase 2 (failure): kills the spawned child, joins the reader tasks (so
    /// the stderr buffer flushes), removes the in-flight `Starting` instance
    /// from the entity's instances list, and returns a formatted error
    /// message including a stderr tail.
    ///
    /// If a concurrent `push_config` replaced the entity wholesale while the
    /// daemon was running its messenger checks, the instance removal is
    /// silently skipped — the new state takes precedence. The child is still
    /// killed either way.
    pub async fn abort_started(
        handle: &Arc<RwLock<NodeEntity>>,
        child: Child,
        started_ctx: StartedInstanceCtx,
        error: String,
        instance_id: &Name,
    ) -> String {
        let StartedInstanceCtx {
            instance_dir,
            runtime_config_path,
            stderr_buffer,
            output_reader_handles,
            log_file,
            generation: start_generation,
        } = started_ctx;

        let msg = kill_and_collect_error(
            child,
            instance_id.as_str(),
            &error,
            stderr_buffer,
            output_reader_handles,
            log_file,
        )
        .await;

        // Best-effort cleanup of the on-disk instance directory and the
        // runtime config temp file. Once the child has been killed and the
        // readers drained, nothing else holds file descriptors into them.
        let _ = std::fs::remove_dir_all(&instance_dir);
        let _ = std::fs::remove_file(&runtime_config_path);

        // Generation guard: only touch the entity's instances list if it is
        // still the same entity we registered against. A concurrent
        // `push_config` would have bumped the generation, in which case the
        // replacement entity owns its own instances and we must not poke at
        // them.
        if handle.read().generation == start_generation {
            Self::remove_starting_instance(handle, instance_id);
        }

        msg
    }

    /// Constructs the root entity for a [`crate::node_stack::NodeStack`]. The
    /// root represents the running daemon itself, not a buildable node — it
    /// bypasses the lifecycle because there is no source to build and no
    /// instance to spawn (the daemon's own process is the "instance").
    ///
    /// This is `pub(crate)` because the only legitimate caller is
    /// `NodeStack::new`.
    pub(crate) fn root(
        config: NodeConfig,
        root_path: PathBuf,
        instance: TrackedNodeInstance,
    ) -> Self {
        Self {
            config,
            stage: NodeStage::Root {
                config_path: root_path,
                instance,
            },
            generation: next_entity_generation(),
            last_add_log_path: None,
        }
    }

    /// Reconstructs an entity from a previously-captured snapshot during stack
    /// restore. The state is taken at face value — no I/O is performed and
    /// the lifecycle is bypassed.
    ///
    /// Used only by [`crate::node_stack::NodeStack::apply_from`] when cloning
    /// state from another stack at startup, where the artifact already exists
    /// on disk and the source instances are still tracked. The resulting
    /// stage is determined by the `(artifact_path, instances)` combination:
    ///
    /// - `(None, [])` → `Added`
    /// - `(Some, [])` → `Ready { instances: [] }`
    /// - `(Some, instances)` → `Ready { instances }` (callers are responsible
    ///   for the instances having `state == Running` — this constructor does
    ///   not enforce it)
    /// - `(None, instances)` → invalid; panics
    pub(crate) fn from_snapshot(
        config: NodeConfig,
        config_path: PathBuf,
        artifact_path: Option<PathBuf>,
        instances: Vec<TrackedNodeInstance>,
    ) -> Self {
        let stage = match (artifact_path, instances.is_empty()) {
            (None, true) => NodeStage::Added { config_path },
            (None, false) => unreachable!(
                "snapshot with instances must have an artifact_path — \
                 a node cannot have instances without a built artifact"
            ),
            (Some(artifact_path), _) => NodeStage::Ready {
                config_path,
                artifact_path,
                instances,
            },
        };
        Self {
            config,
            stage,
            generation: next_entity_generation(),
            last_add_log_path: None,
        }
    }

    /// Removes a `Running` instance from a `Ready` entity. The entity stays
    /// in `Ready` regardless of whether the instance list becomes empty.
    /// `Starting` instances are intentionally left alone — to clean those
    /// up, the caller must use `abort_started`.
    ///
    /// Returns `true` if a `Running` instance was removed, `false` otherwise
    /// (instance missing, in `Starting` state, or entity not in `Ready`).
    pub fn stop_instance(&mut self, instance_id: &Name) -> bool {
        let NodeStage::Ready { instances, .. } = &mut self.stage else {
            return false;
        };
        let Some(pos) = instances.iter().position(|inst| {
            inst.instance_id() == instance_id && inst.state() == InstanceState::Running
        }) else {
            return false;
        };
        let removed = instances.remove(pos);
        // Best-effort cleanup of the on-disk artifacts the start path
        // recorded on the running instance. We ignore errors so a removed
        // file or a missing directory does not block the lifecycle
        // transition.
        if let Some(dir) = removed.instance_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
        if let Some(path) = removed.runtime_config_path() {
            let _ = std::fs::remove_file(path);
        }
        true
    }
}

/// Per-instance state. Lives on `TrackedNodeInstance` rather than on
/// `NodeStage` because each instance manages its own lifecycle: starting and
/// running are properties of *this particular spawn*, not of the parent entity.
///
/// The entity-level [`NodeStage`] only describes artifact readiness; once an
/// entity is in [`NodeStage::Ready`], it can have any combination of
/// `Starting` and `Running` instances in its instances list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    /// `prepare_and_spawn` has registered the instance and the child process
    /// is running, but `commit_started` has not yet been called (the daemon
    /// is still running its messenger-bound ready/health checks). Visible to
    /// observers via `entity.instances()` so they can see "this instance is
    /// currently starting" — but `find_by_instance_id` skips it because
    /// messenger services like `SHUTDOWN_SERVICE` haven't subscribed yet.
    Starting,
    /// `commit_started` has flipped the state. The instance is fully
    /// registered and reachable through messenger services.
    Running,
}

#[derive(Debug, Clone)]
pub struct TrackedNodeInstance {
    instance_id: Name,
    /// Process ID of the running instance. This is `None` for instances running on remote
    /// locations (e.g., embedded systems) where a local PID is not available.
    pid: Option<u32>,
    state: InstanceState,
    /// On-disk instance directory created during start (extracted archive
    /// or freshly-created container instance dir). Persisted on the
    /// `Running` instance so `stop_instance` can clean it up. `None` for
    /// snapshot-restored or test-fixture instances.
    instance_dir: Option<PathBuf>,
    /// On-disk runtime config temp file created by `write_runtime_config_temp`.
    /// Persisted so it can be removed when the instance stops or aborts. `None`
    /// for snapshot-restored or test-fixture instances.
    runtime_config_path: Option<PathBuf>,
}

impl TrackedNodeInstance {
    /// Constructs a new tracked instance. The `state` must be supplied
    /// explicitly — there is no default. Callers that have just spawned a
    /// child process and have not yet committed it pass `InstanceState::Starting`;
    /// callers that are reconstructing an entity from a snapshot or test
    /// fixture pass `InstanceState::Running`.
    pub fn new(instance_id: Name, pid: Option<u32>, state: InstanceState) -> Self {
        Self {
            instance_id,
            pid,
            state,
            instance_dir: None,
            runtime_config_path: None,
        }
    }

    pub fn instance_id(&self) -> &Name {
        &self.instance_id
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn state(&self) -> InstanceState {
        self.state
    }

    /// Returns the on-disk instance directory recorded during start, if any.
    pub fn instance_dir(&self) -> Option<&Path> {
        self.instance_dir.as_deref()
    }

    /// Returns the on-disk runtime config temp file recorded during start,
    /// if any.
    pub fn runtime_config_path(&self) -> Option<&Path> {
        self.runtime_config_path.as_deref()
    }

    /// Same-module mutator used by `NodeEntity::commit_started` to flip a
    /// `Starting` instance to `Running` and record its pid plus the on-disk
    /// paths produced by `prepare_and_spawn`. Not exported.
    fn set_running(
        &mut self,
        pid: Option<u32>,
        instance_dir: PathBuf,
        runtime_config_path: PathBuf,
    ) {
        self.state = InstanceState::Running;
        self.pid = pid;
        self.instance_dir = Some(instance_dir);
        self.runtime_config_path = Some(runtime_config_path);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencySpec {
    pub node_name: String,
    pub node_tag: String,
}
