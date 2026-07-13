use parking_lot::{Mutex as StdMutex, RwLock};
use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use config::node::NodeConfig;
use config::runtime::Name;
use core_node_api::{
    InstanceState, NodeStage as SerializedNodeStage, SerializedInstance, SerializedNode,
};
use daemon_config::consts::PeppyDirs;
use tokio::process::Child;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks, spawn_output_reader_async};
use crate::error::{Error, Result};

use super::build_steps::{
    ContainerBuildInputs, archive_dir_to_storage, build_container_image, move_sif_to_storage,
    run_build_cmd,
};
use super::run_steps::{
    SpawnCommand, SpawnContainerInputs, build_container_command, build_process_command,
    create_instance_dir, extract_node_archive, kill_and_collect_error,
};

impl From<&NodeEntity> for SerializedNode {
    fn from(entity: &NodeEntity) -> Self {
        let all_instances = entity.instances();
        Self {
            name: entity.config().manifest.name.as_str().to_string(),
            tag: entity.config().manifest.tag.clone(),
            config_path: entity.config_path().display().to_string(),
            artifact_path: entity.artifact_path().map(|p| p.display().to_string()),
            stage: Some(entity.stage().to_serialized()),
            instances: all_instances
                .iter()
                .map(|i| SerializedInstance {
                    instance_id: i.instance_id().as_str().to_string(),
                    state: i.state(),
                    healthy: i.healthy(),
                    slot_bindings: i.slot_bindings().clone(),
                    // Filled by the graph-level overlay in
                    // `NodeStackInner::to_serialized_graph` (manifest +
                    // pairing registry); the entity alone cannot know pair
                    // state.
                    pairing_slots: std::collections::BTreeMap::new(),
                })
                .collect(),
        }
    }
}

/// Lifecycle stage of a `NodeEntity`. Describes *artifact readiness only*.
/// Per-instance state (Starting, Running, or a terminal Finished/Failed) lives
/// on each [`TrackedNodeInstance`] inside `Ready.instances`.
///
/// - `Added`: config registered, no artifact, no instances.
/// - `Building`: `build()` is running its I/O. Acts as the concurrency
///   barrier: a second concurrent `build()` on the same entity sees this
///   stage and is rejected immediately with no queueing.
/// - `Ready`: artifact is on disk. The instances list may be empty (no
///   instances spawned yet, equivalent to the old `Built` stage) or hold any
///   mix of `Starting` (in-flight `prepare_and_spawn`), `Running`, and terminal
///   `Finished`/`Failed` instances. A self-exited instance stays listed as
///   `Finished` or `Failed` until the stack is cleared or it is stopped.
/// - `Root`: the synthetic daemon entity. Has no buildable artifact and
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
        self.to_serialized().as_str()
    }

    /// Strips the rich per-stage data and returns the label-only view used on
    /// the wire (in `SerializedNode::stage` and `NodeInfo::stage`).
    pub fn to_serialized(&self) -> SerializedNodeStage {
        match self {
            NodeStage::Added { .. } => SerializedNodeStage::Added,
            NodeStage::Building { .. } => SerializedNodeStage::Building,
            NodeStage::Ready { .. } => SerializedNodeStage::Ready,
            NodeStage::Root { .. } => SerializedNodeStage::Root,
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
    /// of [`NodeStage::ensure_buildable`]; see its doc for the rationale.
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
    /// user-defined `build_cmd` is executed inside this directory before
    /// archiving. The build artifact is produced inside this directory and
    /// then moved to peppy storage.
    pub working_dir: &'a Path,
    /// Resolved peppy directory layout. The built `.sif`/archive is placed
    /// inside `peppy_dirs.built_nodes_dir()`.
    pub peppy_dirs: &'a PeppyDirs,
    /// Channel that streams stdout/stderr lines from the build child process
    /// (and from `build_cmd`, for process nodes) back to the caller.
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    /// Log file the build output is also written to.
    pub log_file: Arc<StdMutex<File>>,
    /// Environment variables passed to `build_cmd` (process nodes only). The
    /// daemon prepares this list via `validate_goal_env_vars`,
    /// `inject_rust_build_env`, and `inject_node_runtime_env`. Container
    /// nodes ignore this field; apptainer build does not consume it.
    pub env_vars: &'a [(String, String)],
    /// Fired when a `--force` build supersedes this one. The build I/O layer
    /// SIGKILLs and reaps the build subprocess so the superseding build can
    /// reuse the working dir without racing a dying process.
    pub cancel_token: CancellationToken,
}

/// Inputs required to drive [`NodeEntity::prepare_and_spawn`] to completion.
///
/// Mirrors [`BuildContext`] for the start path. The struct holds borrows so
/// callers don't have to clone heavy state. Messenger-bound parameters
/// (`signal_target`, ready/health checks) live in the daemon, not here.
pub struct StartContext<'a> {
    /// Instance identifier, used for log messages, runtime-config file
    /// naming, and the eventual `TrackedNodeInstance::new`.
    pub instance_id: &'a Name,
    /// The runtime config to write to the per-spawn temp file. For container
    /// nodes, the caller is responsible for any host_gateway rewriting before
    /// calling start (the entity treats this as opaque bytes).
    pub runtime_config_json5: &'a str,
    /// Producers bound to each of this instance's `depends_on` slots,
    /// recorded on the `TrackedNodeInstance` so the daemon can surface
    /// them via `node_info`. The launcher / CLI compute this from the
    /// validator's per-slot resolution before spawning.
    pub slot_bindings: config::runtime::SlotBindings,
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
    /// Output-pipeline plumbing. The entity does not inspect these fields;
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
    /// themselves stay alive; only their forwarding is disabled.
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
    pub(crate) instance_dir: PathBuf,
    pub(crate) runtime_config_path: PathBuf,
    pub(crate) stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
    pub(crate) output_reader_handles: Vec<JoinHandle<std::io::Result<()>>>,
    pub(crate) log_file: Arc<StdMutex<File>>,
    /// Snapshot of the entity's `generation` taken at `prepare_and_spawn`
    /// time. `commit_started`/`abort_started` compare this against the
    /// current entity generation and refuse to mutate the replacement entity
    /// if a concurrent `push_config` has bumped the generation in the
    /// meantime; they only clean up the stale child/context.
    pub(crate) generation: u64,
}

/// Process-wide monotonic counter that assigns each `NodeEntity` instance a
/// unique generation. The build path snapshots this when transitioning into
/// `Building`, and rejects the publish if the entity it observes after I/O
/// has a different generation, i.e. a concurrent `push_config` replaced the
/// entity contents in the meantime.
static NEXT_ENTITY_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_entity_generation() -> u64 {
    NEXT_ENTITY_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// RAII guard for a temporary working directory staged by `node add` and
/// consumed by `node build`. Removes the directory when the last clone is
/// dropped, so removing an `Added` entity from the stack also cleans up
/// its pending working dir on disk.
#[derive(Debug)]
pub struct WorkingDirGuard {
    path: PathBuf,
}

impl WorkingDirGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkingDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
pub struct NodeEntity {
    config: NodeConfig,
    stage: NodeStage,
    /// Unique-per-construction token used to detect when an in-flight `build`
    /// is racing against a wholesale entity replacement (`push_config_impl`'s
    /// in-place overwrite). Bumped on every `new`, `from_snapshot`, and
    /// `root` construction. The build path captures this value at Phase 1 and
    /// re-checks it at Phase 3 before publishing artifacts.
    generation: u64,
    /// In-memory only: the temporary working directory staged by `node add`
    /// and consumed by `node build`. Set to `Some` while the entity is
    /// `Added`, taken (set to `None`) by the build path. The `Arc` lets the
    /// build task hold its own reference for the duration of the build after
    /// `take_pending_working_dir` clears the entity-side slot. Never
    /// persisted.
    pending_working_dir: Option<Arc<WorkingDirGuard>>,
    /// Broadcasts this entity's stage label on every stage transition
    /// (`Added`/`Building`/`Ready`/`Root`). Observers obtain a receiver via
    /// [`NodeEntity::subscribe_stage`] and react to transitions without
    /// polling. Only the stage *label* is published; per-instance changes that
    /// keep the entity in `Ready` (e.g. `Starting` to `Running`) are not stage
    /// transitions and are not signalled. A wholesale entity replacement
    /// (`push_config`) drops this sender, closing existing receivers.
    stage_tx: watch::Sender<SerializedNodeStage>,
}

impl NodeEntity {
    /// Shared constructor: assigns a fresh generation token and opens the
    /// stage-broadcast channel seeded with `stage`'s label. All three public
    /// constructors (`new`, `root`, `from_snapshot`) route through here so the
    /// generation bump and the watch initialization cannot drift apart.
    fn with_stage(config: NodeConfig, stage: NodeStage) -> Self {
        let (stage_tx, _) = watch::channel(stage.to_serialized());
        Self {
            config,
            stage,
            generation: next_entity_generation(),
            pending_working_dir: None,
            stage_tx,
        }
    }

    /// Publishes the current stage label to [`subscribe_stage`] receivers.
    /// Call immediately after every `self.stage = ...` assignment so a stage
    /// transition is never silently dropped. Send errors (no live receivers)
    /// are ignored, matching the fire-and-forget nature of the signal.
    ///
    /// [`subscribe_stage`]: Self::subscribe_stage
    fn broadcast_stage(&self) {
        let _ = self.stage_tx.send(self.stage.to_serialized());
    }

    /// Creates a new `NodeEntity` in the [`NodeStage::Added`] stage. The
    /// `config_path` should point at the `peppy.json5` file that supplied
    /// `config`.
    pub fn new<P: Into<PathBuf>>(config: NodeConfig, config_path: P) -> Self {
        Self::with_stage(
            config,
            NodeStage::Added {
                config_path: config_path.into(),
            },
        )
    }

    /// Subscribes to this entity's stage-transition broadcasts. The returned
    /// receiver starts at the current stage label, so a caller that subscribes
    /// after a transition still observes the present stage. Used by observers
    /// (and tests) that need to react to `Added`/`Building`/`Ready`
    /// transitions without polling the entity under a lock.
    pub fn subscribe_stage(&self) -> watch::Receiver<SerializedNodeStage> {
        self.stage_tx.subscribe()
    }

    /// Returns the in-memory working directory guard staged by `node add`
    /// for a future `node build`, if any. The build path consumes this via
    /// [`take_pending_working_dir`].
    pub fn pending_working_dir(&self) -> Option<Arc<WorkingDirGuard>> {
        self.pending_working_dir.clone()
    }

    /// Stores the working-directory guard the add path just created so a
    /// later `node build` can reuse it without re-cloning the source.
    pub fn set_pending_working_dir(&mut self, guard: Arc<WorkingDirGuard>) {
        self.pending_working_dir = Some(guard);
    }

    /// Takes the staged working-directory guard, leaving `None` in place.
    pub fn take_pending_working_dir(&mut self) -> Option<Arc<WorkingDirGuard>> {
        self.pending_working_dir.take()
    }

    /// Takes the staged working-directory guard, but only if the entity's
    /// current generation matches `expected`. Returns `Ok(taken_value)` on
    /// match (the taken value may be `None` if already consumed).
    /// Returns `Err(current_generation)` on mismatch, leaving the slot
    /// untouched.
    pub fn take_pending_working_dir_if_generation(
        &mut self,
        expected: u64,
    ) -> std::result::Result<Option<Arc<WorkingDirGuard>>, u64> {
        if self.generation == expected {
            Ok(self.pending_working_dir.take())
        } else {
            Err(self.generation)
        }
    }

    /// Returns the entity's monotonic generation token. Bumped on every
    /// `new`/`from_snapshot`/`root` construction so the build path can
    /// distinguish "still the entity I started building" from "wholesale
    /// replaced by a concurrent push_config".
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Rolls a `Building` entity back to `Added`, preserving `config_path`, and
    /// re-attaches the staged working-directory guard so a follow-up build can
    /// reuse it. Used by the `node_build` `--force` cancellation path: the
    /// superseded build leaves the entity buildable again rather than removing
    /// it (the staged working dir is the only surviving copy of the source).
    ///
    /// The caller ([`super::NodeStack::rollback_to_added_if_matches`]) holds the
    /// stack + entity write locks and has already verified the entity is
    /// `Building` with a matching generation. Deliberately does NOT bump the
    /// generation: this is the same entity, re-presented as buildable, and the
    /// superseding build captures the current generation via its own lookup.
    pub fn rollback_building_to_added(&mut self, working_dir: Arc<WorkingDirGuard>) {
        let NodeStage::Building { config_path } = &self.stage else {
            debug_assert!(
                false,
                "rollback_building_to_added called on a non-Building entity"
            );
            return;
        };
        self.stage = NodeStage::Added {
            config_path: config_path.clone(),
        };
        self.broadcast_stage();
        self.pending_working_dir = Some(working_dir);
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
    /// `~/.peppy/built_nodes`. `None` until the entity has reached `Ready`,
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

    /// Drives the entity from `Added → Building → Ready`.
    ///
    /// On success, returns the `PathBuf` of the artifact installed in storage.
    /// The caller MUST use this returned path rather than re-reading
    /// `entity.artifact_path()` afterwards, because a concurrent
    /// `push_config` could replace the entity in-place.
    ///
    /// **Failure contract:** on any failure the entity is left in `Building`
    /// (not rolled back to `Added`). The caller owns cleanup, typically by
    /// calling `NodeStack::remove_config`.
    pub async fn build(handle: &Arc<RwLock<NodeEntity>>, ctx: BuildContext<'_>) -> Result<PathBuf> {
        // ---- Phase 1: Added → Building, snapshot inputs (brief write lock) ----
        let (node_name, node_tag, config_path, container_opt, build_cmd, build_generation) = {
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
                guard.config.execution.build_cmd.clone(),
                guard.generation,
            );
            // Atomic transition Added → Building under the same write lock as
            // the validation. Any second concurrent call now sees Building.
            guard.stage = NodeStage::Building { config_path };
            guard.broadcast_stage();
            snapshot
        };

        // ---- Phase 2: I/O without any entity lock ----
        // For container nodes, build the .sif via apptainer.
        // For process nodes, run the user-defined build_cmd (if any).
        // Defer publishing the artifact into shared storage until *after*
        // we re-confirm the entity is still `Building` under the write
        // lock; otherwise a stale build could orphan/overwrite an artifact
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
                    peppy_dirs: ctx.peppy_dirs,
                    cancel_token: &ctx.cancel_token,
                })
                .await
                .map_err(|reason| Error::BuildFailed {
                    node_name: node_name.clone(),
                    node_tag: node_tag.clone(),
                    reason,
                })?;
            } else {
                // Process node: run build_cmd inside the working dir.
                run_build_cmd(
                    build_cmd.as_ref(),
                    ctx.working_dir,
                    ctx.env_vars,
                    ctx.feedback_tx,
                    Arc::clone(&ctx.log_file),
                    &ctx.cancel_token,
                )
                .await
                .map_err(|reason| Error::BuildFailed {
                    node_name: node_name.clone(),
                    node_tag: node_tag.clone(),
                    reason: format!("build_cmd failed: {}", reason),
                })?;
            }
            Ok(())
        }
        .await;

        // ---- Phase 2.5: publish the artifact into shared storage ----
        // This is blocking I/O (tar+zstd or fs::copy on potentially multi-GB
        // images) so we run it via `spawn_blocking` off the tokio runtime
        // worker. Done *before* acquiring the phase 3 write lock so the
        // parking_lot guard is never held across blocking I/O.
        let publish_result: std::result::Result<Option<PathBuf>, Error> = match &io_result {
            Ok(()) => {
                let working_dir = ctx.working_dir.to_path_buf();
                let peppy_dirs = ctx.peppy_dirs.clone();
                let node_name_pub = node_name.clone();
                let node_tag_pub = node_tag.clone();
                let join_res = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
                    if is_container {
                        move_sif_to_storage(
                            &working_dir,
                            &node_name_pub,
                            &node_tag_pub,
                            &peppy_dirs,
                        )
                    } else {
                        archive_dir_to_storage(
                            &working_dir,
                            &node_name_pub,
                            &node_tag_pub,
                            &peppy_dirs,
                        )
                    }
                })
                .await;
                match join_res {
                    Ok(Ok(path)) => Ok(Some(path)),
                    Ok(Err(e)) => Err(Error::BuildFailed {
                        node_name: node_name.clone(),
                        node_tag: node_tag.clone(),
                        reason: if is_container {
                            format!("failed to move container image to storage: {}", e)
                        } else {
                            format!("failed to archive node directory: {}", e)
                        },
                    }),
                    Err(e) => Err(Error::BuildFailed {
                        node_name: node_name.clone(),
                        node_tag: node_tag.clone(),
                        reason: format!("storage publish task failed: {}", e),
                    }),
                }
            }
            Err(_) => Ok(None),
        };

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
                let artifact_path =
                    publish_result?.expect("publish_result is Some when io_result is Ok");

                guard.stage = NodeStage::Ready {
                    config_path,
                    artifact_path: artifact_path.clone(),
                    instances: Vec::new(),
                };
                guard.broadcast_stage();
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
    /// removed). Also a no-op if the instance is in `Running` state; that
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

    /// Records the spawned child's `pid` on the still-`Starting` instance
    /// `instance_id`. Called by [`prepare_and_spawn`] while the entity write lock
    /// is already held, in the SAME critical section as the fork, so a daemon
    /// teardown snapshot can never observe a live child whose pid is not yet
    /// force-killable. A no-op if the entity is no longer `Ready` or the instance
    /// is gone, which cannot happen while the `Starting` instance is registered:
    /// `push_config`/`remove_config` both refuse a non-empty instances list under
    /// this same entity write lock, so the entity is neither replaced nor
    /// detached for the lifetime of the start.
    fn record_starting_pid(&mut self, instance_id: &Name, pid: u32) {
        let NodeStage::Ready { instances, .. } = &mut self.stage else {
            return;
        };
        if let Some(inst) = instances.iter_mut().find(|inst| {
            inst.instance_id() == instance_id && inst.state() == InstanceState::Starting
        }) {
            inst.set_starting_pid(pid);
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
            // append below; both happen under this write lock.
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
                ctx.slot_bindings.clone(),
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

        // ---- Phase 3a: build the spawn command (slow, no entity lock) ----
        // All the expensive setup (Apptainer/Lima init, archive extraction,
        // mounts, command construction) runs here without holding any lock; the
        // returned command is forked under the entity write lock in Phase 3b.
        let SpawnCommand {
            mut command,
            runtime_config_path,
            description: spawn_description,
        } = if let Some(container) = node_config.execution.container.as_ref() {
            let apptainer_run_extra_args = container
                .apptainer_run_extra_args
                .as_deref()
                .unwrap_or_default();
            let lima_shell_extra_args = container
                .lima_shell_extra_args
                .as_deref()
                .unwrap_or_default();
            build_container_command(SpawnContainerInputs {
                sif_path: &artifact_path,
                working_dir: &instance_dir,
                instance_id: instance_id_str,
                runtime_config_json5: ctx.runtime_config_json5,
                env_vars: ctx.env_vars,
                mount_paths: ctx.mount_paths_resolved,
                apptainer_run_extra_args,
                lima_shell_extra_args,
                log_file: &ctx.output_sinks.log_file,
                feedback_tx: &ctx.output_sinks.feedback_tx,
                peppy_dirs: ctx.peppy_dirs,
            })
            .await
        } else {
            build_process_command(
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
            // The child never spawned, so nothing else references it; the
            // build helper already cleaned up its own runtime config temp.
            let _ = std::fs::remove_dir_all(&instance_dir);
            Self::remove_starting_instance(handle, ctx.instance_id);
            Error::StartFailed {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                reason: format!("failed to build spawn command: {}", e),
            }
        })?;

        // ---- Phase 3b: fork the child and record its pid atomically ----
        // Fork and record the pid in a single critical section under the entity
        // write lock, so a concurrent daemon teardown can never snapshot a live
        // child whose pid is not yet force-killable. The lock is held across the
        // synchronous `command.spawn()` only (no `.await`); all slow work ran
        // lock-free in Phase 3a. The `Starting` instance is guaranteed present:
        // `push_config`/`remove_config` both refuse a non-empty instances list
        // under this same lock, so the entity is neither replaced nor detached
        // for the lifetime of the start.
        let mut child = {
            let mut guard = handle.write();
            command.spawn().inspect(|child| {
                if let Some(pid) = child.id() {
                    guard.record_starting_pid(ctx.instance_id, pid);
                }
            })
        }
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&instance_dir);
            let _ = std::fs::remove_file(&runtime_config_path);
            Self::remove_starting_instance(handle, ctx.instance_id);
            Error::StartFailed {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                reason: format!("failed to spawn `{}`: {}", spawn_description, e),
            }
        })?;

        // ---- Phase 4: wire output streaming ----
        let stderr_buffer = Arc::new(StdMutex::new(VecDeque::with_capacity(
            crate::build_io::STDERR_TAIL_LINES,
        )));
        let mut output_reader_handles = Vec::new();

        let sinks = &ctx.output_sinks;
        // Register each reader before launching its task so the daemon's drain
        // primitive counts it without racing the task's startup.
        if let Some(stdout) = child.stdout.take() {
            sinks.hooks.on_reader_registered();
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
            sinks.hooks.on_reader_registered();
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
    /// transitions `Starting → Running`. Returns the live `Child` so the caller
    /// can watch it for exit (see the process-exit watcher in `node_run`),
    /// turning a self-exit into a terminal `Finished`/`Failed` instance state.
    ///
    /// Does NOT join the output reader handles; they remain alive past return
    /// so the daemon keeps streaming the running node's stdout/stderr.
    ///
    /// If a concurrent `push_config` replaced the entity wholesale while the
    /// daemon was running its messenger checks, this returns
    /// [`Error::InvalidStageTransition`] **and kills the spawned child** so no
    /// orphan process is left behind. On the success path the `Child` is handed
    /// back (not killed, no `kill_on_drop`), so the OS process keeps running; the
    /// caller owns it from here, holding it in the exit watcher and reaping it on
    /// exit, while `stop_instance` still drives termination by pid.
    pub async fn commit_started(
        handle: &Arc<RwLock<NodeEntity>>,
        mut child: Child,
        started_ctx: StartedInstanceCtx,
        instance_id: Name,
    ) -> Result<Child> {
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
                // Successful commit: hand the live Child back without killing.
                // The OS process keeps running; the caller moves it into the
                // exit watcher, which observes the eventual exit (clean or
                // crash) and reaps the process.
                Ok(child)
            }
            Err(e) => {
                // Concurrent push_config / stale generation / inconsistent
                // state: the entity is no longer ours, but the spawned
                // process and the on-disk artifacts created during this
                // start *are*; kill the child and clean up the temp files
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
    /// silently skipped; the new state takes precedence. The child is still
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
    /// root represents the running daemon itself, not a buildable node; it
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
        Self::with_stage(
            config,
            NodeStage::Root {
                config_path: root_path,
                instance,
            },
        )
    }

    /// Test-only constructor that materializes an entity directly in a target
    /// stage, taking the state at face value: no I/O is performed and the
    /// build/spawn lifecycle is bypassed. Fixtures use it to stand up `Added`
    /// or `Ready` entities (with already-`Running` instances) without driving
    /// the real `build()` / `prepare_and_spawn` paths. The resulting stage is
    /// determined by the `(artifact_path, instances)` combination:
    ///
    /// - `(None, [])` → `Added`
    /// - `(Some, [])` → `Ready { instances: [] }`
    /// - `(Some, instances)` → `Ready { instances }` (callers set each
    ///   instance's state: `Running`, or the terminal `Finished`/`Failed` for
    ///   fixtures that exercise self-exited nodes. This constructor does not
    ///   enforce it)
    /// - `(None, instances)` → invalid; panics
    #[cfg(test)]
    pub(crate) fn from_snapshot(
        config: NodeConfig,
        config_path: PathBuf,
        artifact_path: Option<PathBuf>,
        instances: Vec<TrackedNodeInstance>,
    ) -> Self {
        let stage = match (artifact_path, instances.is_empty()) {
            (None, true) => NodeStage::Added { config_path },
            (None, false) => unreachable!(
                "snapshot with instances must have an artifact_path; \
                 a node cannot have instances without a built artifact"
            ),
            (Some(artifact_path), _) => NodeStage::Ready {
                config_path,
                artifact_path,
                instances,
            },
        };
        Self::with_stage(config, stage)
    }

    /// Transitions a `Running` instance of a `Ready` entity to a terminal state
    /// after its process has exited on its own: `Finished` when `success` (a
    /// clean exit), `Failed` otherwise (a crash). The instance stays in the
    /// entity so it remains visible in `stack list`; it is removed only when the
    /// stack is cleared or the instance is explicitly stopped.
    ///
    /// No-op (returns `None`) if the instance is missing, not `Running`, marked
    /// `stopping` (a stop path owns its removal, so an intentional kill is never
    /// shown as a crash), or the entity is not `Ready`. Returns the new terminal
    /// state when the transition was applied. The `stopping` check and the state
    /// flip happen together under the caller's write lock, so they cannot race a
    /// concurrent stop.
    pub fn mark_instance_exited(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        success: bool,
    ) -> Option<InstanceState> {
        let mut guard = handle.write();
        let NodeStage::Ready { instances, .. } = &mut guard.stage else {
            return None;
        };
        let inst = instances.iter_mut().find(|inst| {
            inst.instance_id() == instance_id && inst.state() == InstanceState::Running
        })?;
        if inst.is_stopping() {
            return None;
        }
        inst.set_exited(success);
        Some(inst.state())
    }

    /// Removes a `Running` or terminal (`Finished`/`Failed`) instance from a
    /// `Ready` entity. The entity stays in `Ready` regardless of whether the
    /// instance list becomes empty. `Starting` instances are intentionally left
    /// alone; to clean those up, the caller must use `abort_started`.
    ///
    /// Terminal instances are removable here so an explicit stop (or a stack
    /// clear) can clear out a one-shot node that already exited on its own, and
    /// so a self-exit that raced the stop path's removal cannot leave an
    /// untracked instance behind.
    ///
    /// Returns `true` if an instance was removed, `false` otherwise (instance
    /// missing, in `Starting` state, or entity not in `Ready`).
    pub fn stop_instance(&mut self, instance_id: &Name) -> bool {
        let NodeStage::Ready { instances, .. } = &mut self.stage else {
            return false;
        };
        let Some(pos) = instances.iter().position(|inst| {
            inst.instance_id() == instance_id && inst.state() != InstanceState::Starting
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
    /// Producers bound to each of this consumer instance's `depends_on`
    /// slots, mirroring
    /// [`config::runtime::NodeInstanceConfig::slot_bindings`].
    /// Surfaced through `node_info` so the launcher / CLI can
    /// cross-check newly-staged binding plans against running
    /// consumers' existing claims. Empty when the node has no
    /// `depends_on` slots.
    slot_bindings: config::runtime::SlotBindings,
    /// Last `node_health` outcome recorded by the daemon's health monitor.
    /// Behind an `Arc<AtomicBool>` so the monitor can update it through the
    /// cheap clone returned by `NodeStack::find_by_instance_id`, without taking
    /// an entity write lock. `true` until a probe is observed to fail; surfaced
    /// by `stack list` so it reports health without a per-instance round-trip.
    healthy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the stop paths before they terminate this instance's process, so
    /// the process-exit watcher can tell a daemon-initiated stop (cooperative
    /// shutdown or force-kill) apart from a self-exit. When set, the watcher
    /// leaves the state alone and lets the stop path own removal, rather than
    /// transitioning the instance to a terminal `Finished`/`Failed` state (and
    /// mislabeling an intentional force-kill as a crash). Behind an
    /// `Arc<AtomicBool>` for the same reason as `healthy`: it is flipped through
    /// the clone the stop path resolves, without an entity write lock.
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TrackedNodeInstance {
    /// Constructs a new tracked instance. The `state` must be supplied
    /// explicitly; there is no default. Callers that have just spawned a
    /// child process and have not yet committed it pass `InstanceState::Starting`;
    /// callers that are reconstructing an entity from a snapshot or test
    /// fixture pass `InstanceState::Running`. `slot_bindings` carries the
    /// validator-resolved per-slot bindings for this instance; pass an
    /// empty map when reconstructing test fixtures or instances whose
    /// manifest has no `depends_on` slots.
    pub fn new(
        instance_id: Name,
        pid: Option<u32>,
        state: InstanceState,
        slot_bindings: config::runtime::SlotBindings,
    ) -> Self {
        Self {
            instance_id,
            pid,
            state,
            instance_dir: None,
            runtime_config_path: None,
            slot_bindings,
            healthy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            stopping: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Returns the validator-resolved per-slot bindings recorded for
    /// this instance. Empty for instances whose manifest has no
    /// `depends_on` slots or for snapshot-restored / test-fixture
    /// instances built with an empty bindings map.
    pub fn slot_bindings(&self) -> &config::runtime::SlotBindings {
        &self.slot_bindings
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

    /// The last `node_health` outcome the health monitor recorded for this
    /// instance. `true` until a probe is observed to fail.
    pub fn healthy(&self) -> bool {
        self.healthy.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Records the latest `node_health` outcome. Takes `&self` because the flag
    /// is an `Arc<AtomicBool>`; the health monitor updates it through the clone
    /// returned by `NodeStack::find_by_instance_id`.
    pub fn set_healthy(&self, healthy: bool) {
        self.healthy
            .store(healthy, std::sync::atomic::Ordering::Relaxed);
    }

    /// `true` once a stop path has claimed this instance for termination via
    /// [`mark_stopping`]. The process-exit watcher reads this to avoid
    /// transitioning an intentionally-stopped instance to a terminal state.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Marks this instance as being deliberately stopped, before its process is
    /// signaled. Takes `&self` because the flag is an `Arc<AtomicBool>`; the
    /// stop path flips it through the clone it resolves. Idempotent.
    pub fn mark_stopping(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Relaxed);
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

    /// Same-module mutator that records the spawned child's pid on an instance
    /// still in `Starting`, ahead of `commit_started` flipping it to `Running`
    /// via `set_running`. Lets a daemon teardown during the start window reach
    /// the child's process group. Not exported.
    fn set_starting_pid(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    /// Same-module mutator used by `NodeEntity::mark_instance_exited` to flip a
    /// `Running` instance to a terminal state once its process has exited on its
    /// own: `Finished` for a clean exit, `Failed` for a crash. The pid is
    /// cleared because the process is gone, so nothing downstream tries to
    /// signal a stale (and possibly reused) pid. Not exported.
    fn set_exited(&mut self, success: bool) {
        self.state = if success {
            InstanceState::Finished
        } else {
            InstanceState::Failed
        };
        self.pid = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sensor_config() -> NodeConfig {
        serde_json5::from_str::<NodeConfig>(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "sensor", tag: "v1" },
                interfaces: {},
                execution: { language: "rust", run_cmd: ["sensor"] }
            }"#,
        )
        .expect("valid sensor config")
    }

    /// Guards the one line that places resolved bindings onto the `graph_json`
    /// wire: `From<&NodeEntity>` must copy each instance's `slot_bindings`
    /// through to its `SerializedInstance`. Reverting that line to an empty map
    /// makes this fail.
    #[test]
    fn serialized_node_carries_per_instance_slot_bindings() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "arm".to_string(),
            config::runtime::ProducerRef::new("core_a", "arm-1"),
        );
        let bound = TrackedNodeInstance::new(
            Name::new("sensor-1").unwrap(),
            Some(42),
            InstanceState::Running,
            bindings.clone(),
        );
        // A second, bindless instance must round-trip as an empty map.
        let unbound = TrackedNodeInstance::new(
            Name::new("sensor-2").unwrap(),
            Some(43),
            InstanceState::Running,
            BTreeMap::new(),
        );
        let entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            vec![bound, unbound],
        );

        let serialized = SerializedNode::from(&entity);
        assert_eq!(serialized.instances.len(), 2);
        assert_eq!(serialized.instances[0].instance_id, "sensor-1");
        assert_eq!(serialized.instances[0].slot_bindings, bindings);
        assert!(serialized.instances[1].slot_bindings.is_empty());
    }

    /// Guards the one line that places per-instance health onto the
    /// `graph_json` wire: `From<&NodeEntity>` must copy each instance's
    /// `healthy()` through to its `SerializedInstance`. Hardcoding that line to
    /// `true` (or dropping it) makes the unhealthy assertion fail. Also the
    /// only coverage of the `healthy()`/`set_healthy()` pair: a fresh instance
    /// defaults to healthy, and `set_healthy(false)` flips it.
    #[test]
    fn serialized_node_carries_per_instance_health() {
        let healthy = TrackedNodeInstance::new(
            Name::new("sensor-1").unwrap(),
            Some(42),
            InstanceState::Running,
            BTreeMap::new(),
        );
        assert!(
            healthy.healthy(),
            "a freshly-created instance should default to healthy"
        );
        let unhealthy = TrackedNodeInstance::new(
            Name::new("sensor-2").unwrap(),
            Some(43),
            InstanceState::Running,
            BTreeMap::new(),
        );
        unhealthy.set_healthy(false);
        assert!(
            !unhealthy.healthy(),
            "set_healthy(false) should flip the flag"
        );

        let entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            vec![healthy, unhealthy],
        );

        let serialized = SerializedNode::from(&entity);
        assert_eq!(serialized.instances.len(), 2);
        assert!(
            serialized.instances[0].healthy,
            "healthy instance should serialize as healthy"
        );
        assert!(
            !serialized.instances[1].healthy,
            "unhealthy instance should serialize as unhealthy"
        );
    }

    /// Builds a single-`Running`-instance `Ready` entity behind a handle, the
    /// shape `mark_instance_exited` operates on.
    fn ready_entity_with(instance: TrackedNodeInstance) -> Arc<RwLock<NodeEntity>> {
        let entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            vec![instance],
        );
        Arc::new(RwLock::new(entity))
    }

    #[test]
    fn mark_instance_exited_moves_running_to_finished_on_clean_exit() {
        let id = Name::new("one-shot-1").unwrap();
        let handle = ready_entity_with(TrackedNodeInstance::new(
            id.clone(),
            Some(7),
            InstanceState::Running,
            BTreeMap::new(),
        ));

        let new_state = NodeEntity::mark_instance_exited(&handle, &id, true);
        assert_eq!(new_state, Some(InstanceState::Finished));
        {
            let guard = handle.read();
            let inst = &guard.instances()[0];
            assert_eq!(inst.state(), InstanceState::Finished);
            assert_eq!(inst.pid(), None, "pid is cleared once the process is gone");
        }

        // A second exit is a no-op: the instance is already terminal.
        assert_eq!(NodeEntity::mark_instance_exited(&handle, &id, false), None);
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );
    }

    #[test]
    fn mark_instance_exited_moves_running_to_failed_on_unclean_exit() {
        let id = Name::new("crash-1").unwrap();
        let handle = ready_entity_with(TrackedNodeInstance::new(
            id.clone(),
            Some(9),
            InstanceState::Running,
            BTreeMap::new(),
        ));

        assert_eq!(
            NodeEntity::mark_instance_exited(&handle, &id, false),
            Some(InstanceState::Failed)
        );
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Failed);
    }

    #[test]
    fn mark_instance_exited_is_noop_for_an_instance_being_stopped() {
        let id = Name::new("stopping-1").unwrap();
        let instance = TrackedNodeInstance::new(
            id.clone(),
            Some(11),
            InstanceState::Running,
            BTreeMap::new(),
        );
        // A stop path has claimed this instance; the watcher must not relabel
        // the intentional exit as a self-exit.
        instance.mark_stopping();
        let handle = ready_entity_with(instance);

        assert_eq!(NodeEntity::mark_instance_exited(&handle, &id, true), None);
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Running,
            "a stopping instance stays Running so the stop path can remove it"
        );
    }

    #[test]
    fn stop_instance_removes_a_terminal_instance() {
        // A one-shot node that already finished on its own can still be cleared
        // by the stop path (and a self-exit that raced removal cannot leak).
        let id = Name::new("done-1").unwrap();
        let mut entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            vec![TrackedNodeInstance::new(
                id.clone(),
                None,
                InstanceState::Finished,
                BTreeMap::new(),
            )],
        );
        assert!(entity.stop_instance(&id), "terminal instance is removable");
        assert!(entity.instances().is_empty());
    }
}
