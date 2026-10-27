use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use config::node::{Cardinality, ContainerConfig, NodeConfig, PeppygenLanguage};
use config::runtime::Name;
use core_node_api::encoding::{LaunchIdentity, NodeRunGoal, ObservationTargets};
use core_node_api::{
    ExitCause, InstanceEndpoint, InstanceState, NodeStage as SerializedNodeStage, RestartStatus,
    SerializedInstance, SerializedNode,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::PackagesBaseUrl;
use daemon_config::repository::PinnedDocument;
use log_export::Iostream;
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::action_log::Announcer;
use crate::build_io::{OutputReaderHooks, StderrTail, spawn_output_reader_async};
use crate::error::{Error, Result};

use super::build_artifact_cache::{ArtifactKind, prune_siblings, resolve_slot, reuse_line};
use super::build_steps::{
    ContainerBuildInputs, archive_dir_to_storage, build_container_image, move_sif_to_storage,
    run_build_cmd,
};
use super::pairing::{PlannedPair, SlotAddr};
use super::pypi_mirror;
use super::run_steps::{
    SpawnCommand, SpawnContainerInputs, TempFileGuard, build_built_in_command,
    build_container_command, build_process_command, create_instance_dir, extract_node_archive,
    kill_and_collect_error,
};
use super::supervision::{
    Attempt, ExitRecord, FirstStart, HealthKill, KeptRelations, ProcessClaim, Registrations,
    RespawnInputs, RestartBackoff, SlotLeft, SlotRemoval, SpawnTarget, StartRun, StartingClaim,
    Supervision,
};
use crate::process_group::ProcessGroup;
use crate::time_source::TimeSource;

pub(super) fn serialize_node_entity(entity: &NodeEntity, core_node: &str) -> SerializedNode {
    let all_instances = entity.instances();
    SerializedNode {
        name: entity.config().manifest.name.as_str().to_string(),
        tag: entity.config().manifest.tag.clone(),
        core_node: core_node.to_string(),
        config_path: entity.config_path().display().to_string(),
        artifact_path: entity.artifact_path().map(|p| p.display().to_string()),
        stage: entity.stage().to_serialized(),
        instances: all_instances
            .iter()
            .map(|i| SerializedInstance {
                healthy: i.healthy(),
                clock: i.clock().clone(),
                slot_bindings: i.slot_bindings().clone(),
                // Filled by the graph-level overlay in
                // `NodeStackInner::to_serialized_graph` (manifest + pairing
                // registry); the entity alone cannot know pair state.
                pairing_slots: BTreeMap::new(),
                endpoints: i.endpoints().to_vec(),
                restart_status: i.restart_status(),
                ..SerializedInstance::of(i.instance_id().as_str().to_string(), i.state())
            })
            .collect(),
    }
}

/// How a built-in node starts: the daemon's own executable with a
/// subcommand, plus the environment that hands the process what it serves.
///
/// A built-in node is registered ready to start from documents the daemon
/// derived itself; nothing is fetched, generated or built for it. The
/// recipe is the [`Artifact::BuiltIn`] of its `Ready` stage, and what the
/// spawn runs instead of the manifest's `run_cmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInLaunch {
    /// The executable to run; the entity's artifact path.
    pub executable: PathBuf,
    /// The arguments after the executable.
    pub args: Vec<String>,
    /// Environment entries the process needs beyond the instance's own.
    pub env: Vec<(String, String)>,
}

/// What a `Ready` entity spawns from.
#[derive(Debug, Clone)]
pub enum Artifact {
    /// A built `.sif` or archive in `~/.peppy/built_nodes`, run through the
    /// manifest's `run_cmd`, in a container when the manifest declares one.
    Built(PathBuf),
    /// The daemon's own executable with the recipe that runs it; nothing
    /// was fetched, generated or built for the node.
    BuiltIn(BuiltInLaunch),
}

impl Artifact {
    /// The file on disk the entity runs from: the archive or SIF of a
    /// sourced node, the executable of a built-in one.
    pub fn path(&self) -> &Path {
        match self {
            Self::Built(path) => path,
            Self::BuiltIn(launch) => &launch.executable,
        }
    }

    /// The recipe of a built-in node, `None` for a built artifact.
    pub fn built_in(&self) -> Option<&BuiltInLaunch> {
        match self {
            Self::Built(_) => None,
            Self::BuiltIn(launch) => Some(launch),
        }
    }
}

/// Lifecycle stage of a `NodeEntity`. Describes *artifact readiness only*.
/// Per-instance state (Starting, Running, Restarting, or a terminal
/// Finished/Failed) lives on each [`TrackedNodeInstance`] inside
/// `Ready.instances`.
///
/// - `Added`: config registered, no artifact, no instances.
/// - `Building`: `build()` is running its I/O. Acts as the concurrency
///   barrier: a second concurrent `build()` on the same entity sees this
///   stage and is rejected immediately with no queueing.
/// - `Ready`: the [`Artifact`] is on disk, built from sources or the
///   daemon's own executable for a built-in node. The instances list may be
///   empty (no instances spawned yet, equivalent to the old `Built` stage)
///   or hold any mix of `Starting` (in-flight `prepare_and_spawn`),
///   `Running`, `Restarting`, and terminal `Finished`/`Failed` instances. A
///   self-exited instance its policy does not restart stays listed as
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
        artifact: Artifact,
        instances: Vec<TrackedNodeInstance>,
    },
    Root {
        config_path: PathBuf,
        /// Boxed so the one root entity does not make every stage as large
        /// as a [`TrackedNodeInstance`] (`clippy::large_enum_variant`).
        instance: Box<TrackedNodeInstance>,
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
    /// inside `peppy_dirs.built_node_dir(name, tag)`, named after the
    /// fingerprint of the staged tree.
    pub peppy_dirs: &'a PeppyDirs,
    /// The build log and the channel that streams stdout/stderr lines from
    /// the build child process (and from `build_cmd`, for process nodes)
    /// back to the caller.
    pub announcer: Announcer,
    /// Environment variables passed to `build_cmd` (process nodes only). The
    /// daemon prepares this list via `validate_goal_env_vars`,
    /// `inject_rust_build_env`, and `inject_node_runtime_env`. Container
    /// nodes ignore this field; apptainer build does not consume it.
    pub env_vars: &'a [(String, String)],
    /// Fired when a `--force` build supersedes this one. The build I/O layer
    /// SIGKILLs and reaps the build subprocess so the superseding build can
    /// reuse the working dir without racing a dying process.
    pub cancel_token: CancellationToken,
    /// When set, an artifact already in storage for the staged tree's
    /// fingerprint is ignored and this build's result replaces it.
    pub rebuild: bool,
    /// The PyPI mirror a Python build downloads the PyPI files of its
    /// `uv.lock` from, or `None` for PyPI itself (`pypi_mirror` in
    /// `peppy_config.json5`). Other nodes ignore it.
    pub pypi_mirror: Option<&'a PackagesBaseUrl>,
}

/// What [`NodeEntity::build`] snapshots under its Phase 1 write lock, so the
/// I/O phases run without touching the entity again.
struct BuildSnapshot {
    node_name: String,
    node_tag: String,
    config_path: PathBuf,
    container: Option<ContainerConfig>,
    build_cmd: Option<Vec<String>>,
    language: PeppygenLanguage,
    generation: u64,
}

impl BuildSnapshot {
    fn artifact_kind(&self) -> ArtifactKind {
        if self.container.is_some() {
            ArtifactKind::Container
        } else {
            ArtifactKind::Process
        }
    }

    fn build_failed(&self, reason: String) -> Error {
        Error::BuildFailed {
            node_name: self.node_name.clone(),
            node_tag: self.node_tag.clone(),
            reason,
        }
    }
}

/// Inputs required to drive [`NodeEntity::prepare_and_spawn`] to completion.
///
/// Mirrors [`BuildContext`] for the start path. The struct holds borrows so
/// callers don't have to clone heavy state. Messenger-bound parameters
/// (`signal_target`, ready/health checks) live in the daemon, not here.
pub struct StartContext<'a> {
    /// The daemon's name, which the duplicate-id refusal names after
    /// `--core-node`.
    pub core_node: &'a str,
    /// The run log of the process this start makes. A first start records
    /// it on its new slot; a restart's slot names it since the log opened
    /// (`NodeEntity::record_respawn_log`).
    pub run_log_path: &'a Path,
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
    /// The one clock this instance reads, recorded on the
    /// [`TrackedNodeInstance`] so `stack list` and `node_info` report it and a
    /// later `node run` can hold its connections to the same clock rule a
    /// launch applies.
    pub clock: config::runtime::ClockBinding,
    /// The launch this instance belongs to, recorded on the
    /// [`TrackedNodeInstance`] so a clock domain it publishes names the launch
    /// that owns it. `None` for an instance `peppy node run` started.
    pub launch: Option<LaunchIdentity>,
    /// The copy this instance belongs to, recorded on the
    /// `TrackedNodeInstance` so every pair it takes part in can name it.
    pub copy: Option<config::runtime::Name>,
    /// Whether this start registers a new instance, with its policy and the
    /// goal it keeps, or starts the next process of a `restarting` one.
    pub spawn: SpawnTarget,
    /// User + injected env vars (already passed through
    /// `validate_goal_env_vars`, `inject_rust_build_env`, and
    /// `inject_node_runtime_env` in core-node).
    pub env_vars: &'a [(String, String)],
    /// Mount paths with `${parameters:...}` already resolved by core-node
    /// (against runtime arguments and the blocked-source policy). Container
    /// nodes only.
    pub mount_paths_resolved: &'a [String],
    /// The key file that the daemon wrote for this start of the instance,
    /// which its runtime config names. From here on the start owns it: it
    /// goes with the runtime config file of the process, and a container
    /// sees it alone of the credentials of the daemon.
    pub credential: &'a Path,
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
    /// The run log and the channel that receives stdout/stderr lines from
    /// the running child. The reader tasks remain alive past
    /// `prepare_and_spawn`'s return.
    pub announcer: Announcer,
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

impl OutputSinks {
    /// Registers one output reader with the hooks, then spawns its task.
    /// The reader is registered before its task launches so the daemon's
    /// drain primitive counts it without racing the task's startup.
    fn register_and_spawn_reader<R>(
        &self,
        reader: R,
        stream: Iostream,
        stderr_tail: Option<StderrTail>,
    ) -> JoinHandle<std::io::Result<()>>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        self.hooks.on_reader_registered();
        spawn_output_reader_async(
            reader,
            self.announcer.feedback_tx().clone(),
            Arc::clone(&self.publish_enabled),
            Arc::clone(&self.hooks),
            stream,
            stderr_tail,
            self.announcer.log().clone(),
        )
    }
}

/// The files that one spawn of an instance writes for its process: its
/// runtime config and its key file. Both live as long as that process, and
/// go together when it ends or its start fails.
#[derive(Debug, Clone)]
pub(crate) struct SpawnFiles {
    pub(crate) runtime_config: PathBuf,
    pub(crate) credential: PathBuf,
}

impl SpawnFiles {
    /// Removes both files, best-effort: a file already gone is no error.
    pub(crate) fn remove(&self) {
        let _ = std::fs::remove_file(&self.runtime_config);
        let _ = std::fs::remove_file(&self.credential);
    }
}

/// State handed back from [`NodeEntity::prepare_and_spawn`]. The caller owns
/// it across `.await` boundaries (so it must be `Send`) and passes it to
/// either [`NodeEntity::commit_started`] or [`NodeEntity::abort_started`].
#[derive(Debug)]
pub struct StartedInstanceCtx {
    pub(crate) instance_dir: PathBuf,
    pub(crate) spawn_files: SpawnFiles,
    pub(crate) stderr_buffer: StderrTail,
    pub(crate) output_reader_handles: Vec<JoinHandle<std::io::Result<()>>>,
    /// The process's group, led by the pid recorded at spawn. A startup
    /// probe that sees the process exit reaps it, after which the child no
    /// longer has a pid.
    pub group: ProcessGroup,
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

/// Source of [`TrackedNodeInstance`] incarnations. Every process an instance
/// starts, its first and each restart's, draws a fresh one, so no two
/// processes in the daemon's lifetime share one, even under one instance id.
static NEXT_INCARNATION: AtomicU64 = AtomicU64::new(1);

fn next_incarnation() -> u64 {
    NEXT_INCARNATION.fetch_add(1, Ordering::Relaxed)
}

/// RAII guard for a temporary working directory staged by `node add` and
/// consumed by `node build`. Removes the directory on drop, which for the
/// shared `Arc<WorkingDirGuard>` is when its last clone drops, so removing
/// an `Added` entity from the stack also cleans up its pending working dir
/// on disk.
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
    /// in-place overwrite). Bumped by every constructor (`new`, `built_in`,
    /// `root`, `from_snapshot`). The build path captures this value at Phase 1
    /// and re-checks it at Phase 3 before publishing artifacts.
    generation: u64,
    /// In-memory only: the temporary working directory staged by `node add`
    /// and consumed by `node build`. Set to `Some` while the entity is
    /// `Added`, taken (set to `None`) by the build path. The `Arc` lets the
    /// build task hold its own reference for the duration of the build after
    /// `take_pending_working_dir_if_generation` clears the entity-side slot.
    /// Never persisted.
    pending_working_dir: Option<Arc<WorkingDirGuard>>,
    /// The contract and pairing documents this entity's manifest names, each
    /// beside its pin, at the bytes its code was generated from: the add or
    /// the registration that builds the entity gives them to its
    /// constructor, and they never change after, whatever happens to the
    /// files they were read from. Empty for a node whose manifest names no
    /// document, and for the root.
    documents: Vec<PinnedDocument>,
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
    /// stage-broadcast channel seeded with `stage`'s label. Every public
    /// constructor (`new`, `built_in`, `root`, `from_snapshot`) routes
    /// through here so the generation bump and the watch initialization
    /// cannot drift apart.
    fn with_stage(config: NodeConfig, stage: NodeStage, documents: Vec<PinnedDocument>) -> Self {
        let (stage_tx, _) = watch::channel(stage.to_serialized());
        Self {
            config,
            stage,
            generation: next_entity_generation(),
            pending_working_dir: None,
            documents,
            stage_tx,
        }
    }

    /// Creates a built-in node, ready to start: its artifact is `launch`,
    /// whose executable the spawn runs rather than the manifest's `run_cmd`.
    /// `config_path` points at the manifest the daemon derived and wrote for
    /// it, and `documents` are the documents that manifest names.
    pub fn built_in<P: Into<PathBuf>>(
        config: NodeConfig,
        config_path: P,
        launch: BuiltInLaunch,
        documents: Vec<PinnedDocument>,
    ) -> Self {
        Self::with_stage(
            config,
            NodeStage::Ready {
                config_path: config_path.into(),
                artifact: Artifact::BuiltIn(launch),
                instances: Vec::new(),
            },
            documents,
        )
    }

    /// The spawn recipe of a built-in node, `None` for a sourced node.
    pub fn built_in_launch(&self) -> Option<&BuiltInLaunch> {
        match &self.stage {
            NodeStage::Ready { artifact, .. } => artifact.built_in(),
            _ => None,
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
    /// `config`, and `documents` are the documents its code was generated
    /// from.
    pub fn new<P: Into<PathBuf>>(
        config: NodeConfig,
        config_path: P,
        documents: Vec<PinnedDocument>,
    ) -> Self {
        Self::with_stage(
            config,
            NodeStage::Added {
                config_path: config_path.into(),
            },
            documents,
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
    /// [`Self::take_pending_working_dir_if_generation`].
    pub fn pending_working_dir(&self) -> Option<Arc<WorkingDirGuard>> {
        self.pending_working_dir.clone()
    }

    /// Stores the working-directory guard the add path just created so a
    /// later `node build` can reuse it without re-cloning the source.
    pub fn set_pending_working_dir(&mut self, guard: Arc<WorkingDirGuard>) {
        self.pending_working_dir = Some(guard);
    }

    /// The contract and pairing documents the manifest names, at the bytes
    /// the entity's code was generated from.
    pub fn documents(&self) -> &[PinnedDocument] {
        &self.documents
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

    /// Returns the entity's monotonic generation token. Bumped by every
    /// constructor (`new`, `built_in`, `root`, `from_snapshot`) so the build
    /// path can distinguish "still the entity I started building" from
    /// "wholesale replaced by a concurrent push_config".
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
    pub(crate) fn rollback_building_to_added(&mut self, working_dir: Arc<WorkingDirGuard>) {
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
    /// `~/.peppy/built_nodes`, or a built-in node's executable. `None` until
    /// the entity has reached `Ready`, and `None` for the synthetic root
    /// entity (which has no artifact).
    pub fn artifact_path(&self) -> Option<&Path> {
        match &self.stage {
            NodeStage::Added { .. } | NodeStage::Building { .. } | NodeStage::Root { .. } => None,
            NodeStage::Ready { artifact, .. } => Some(artifact.path()),
        }
    }

    /// Returns the tracked instances of this entity: the `Starting`
    /// (in-flight), `Running` (committed), and terminal `Finished`/`Failed`
    /// instances of a `Ready` entity, or the one daemon instance of the root
    /// entity. Observers that only care about fully-started instances should
    /// filter by [`TrackedNodeInstance::state`]. Returns an empty slice for
    /// `Added` and `Building`.
    pub fn instances(&self) -> &[TrackedNodeInstance] {
        match &self.stage {
            NodeStage::Ready { instances, .. } => instances,
            NodeStage::Root { instance, .. } => std::slice::from_ref(instance.as_ref()),
            NodeStage::Added { .. } | NodeStage::Building { .. } => &[],
        }
    }

    /// Drives the entity from `Added → Building → Ready`.
    ///
    /// A staged tree whose fingerprint already has an artifact in storage is
    /// not rebuilt unless `ctx.rebuild` is set (see
    /// `node_stack::build_artifact_cache`).
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
        let snapshot = {
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
            let snapshot = BuildSnapshot {
                node_name: guard.config.manifest.name.as_str().to_owned(),
                node_tag: guard.config.manifest.tag.clone(),
                config_path: config_path.clone(),
                container: guard.config.execution.container.clone(),
                build_cmd: guard.config.execution.build_cmd.clone(),
                language: guard.config.execution.language,
                generation: guard.generation,
            };
            // Atomic transition Added → Building under the same write lock as
            // the validation. Any second concurrent call now sees Building.
            guard.stage = NodeStage::Building { config_path };
            guard.broadcast_stage();
            snapshot
        };

        // ---- Phase 2: produce the artifact without any entity lock ----
        let build_result = Self::produce_artifact(&snapshot, &ctx).await;

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
            || guard.generation != snapshot.generation
        {
            return Err(Error::InvalidStageTransition {
                node_name: snapshot.node_name,
                node_tag: snapshot.node_tag,
                from: guard.stage.name(),
                to: "Ready",
            });
        }

        // On failure the entity stays in `Building`; the caller owns cleanup
        // (typically `NodeStack::remove_config`). See the failure contract in
        // the doc comment on this method.
        let artifact_path = build_result?;
        guard.stage = NodeStage::Ready {
            config_path: snapshot.config_path,
            artifact: Artifact::Built(artifact_path.clone()),
            instances: Vec::new(),
        };
        guard.broadcast_stage();
        Ok(artifact_path)
    }

    /// Phase 2 of [`NodeEntity::build`]: the artifact for the snapshot's
    /// staged tree, reused from storage when one built from the same tree is
    /// already there, otherwise built and published to its slot. Runs
    /// without any entity lock.
    async fn produce_artifact(snapshot: &BuildSnapshot, ctx: &BuildContext<'_>) -> Result<PathBuf> {
        let kind = snapshot.artifact_kind();

        // Fingerprinting reads every staged file, so it runs on the blocking
        // pool. It happens before any build I/O: the `.sif` apptainer writes
        // into the working dir and whatever `build_cmd` leaves behind must not
        // feed the key of the build producing them.
        let slot = {
            let peppy_dirs = ctx.peppy_dirs.clone();
            let working_dir = ctx.working_dir.to_path_buf();
            let node_name = snapshot.node_name.clone();
            let node_tag = snapshot.node_tag.clone();
            tokio::task::spawn_blocking(move || {
                resolve_slot(&peppy_dirs, &node_name, &node_tag, &working_dir, kind)
            })
            .await
            .map_err(|e| snapshot.build_failed(format!("fingerprint task failed: {e}")))?
            .map_err(|e| {
                snapshot.build_failed(format!(
                    "failed to fingerprint the staged node tree at {}: {e}",
                    ctx.working_dir.display()
                ))
            })?
        };

        if slot.cached && !ctx.rebuild {
            ctx.announcer.line(reuse_line(
                &snapshot.node_name,
                &snapshot.node_tag,
                &slot.fingerprint,
                &slot.path,
            ));
            return Ok(slot.path);
        }
        let line = if slot.cached {
            format!(
                "Rebuilding {}:{} (fingerprint {}); replacing cached build at {}",
                snapshot.node_name,
                snapshot.node_tag,
                slot.fingerprint,
                slot.path.display()
            )
        } else {
            format!(
                "No cached build of {}:{} for fingerprint {}; building",
                snapshot.node_name, snapshot.node_tag, slot.fingerprint
            )
        };
        ctx.announcer.line(line);

        // The staged locks are rewritten after the fingerprint was taken, so
        // a build reuses or replaces the same artifact with and without a
        // mirror. Container and process nodes both build from the staged
        // tree, so both download from the mirror.
        let mirror_feedback = pypi_mirror::BuildFeedback::new(ctx.announcer.clone());
        let rewritten_locks = match ctx.pypi_mirror {
            Some(mirror) if snapshot.language == PeppygenLanguage::Python => {
                pypi_mirror::apply(ctx.working_dir, mirror, &mirror_feedback, &ctx.cancel_token)
                    .await
            }
            _ => pypi_mirror::RewrittenLocks::none(),
        };
        // A build cancelled before it starts its build (during the mirror
        // check, say) stops here instead of starting a child only to kill it.
        if ctx.cancel_token.is_cancelled() {
            rewritten_locks.restore(&mirror_feedback).await;
            return Err(snapshot.build_failed("build cancelled".to_string()));
        }

        let published = Self::build_and_publish(snapshot, ctx, kind, slot.path).await;
        // A failed build puts the staged locks back: a cancelled build hands
        // its staged tree to the next build of the node, which must
        // fingerprint the tree as it was staged. A build that succeeds keeps
        // the rewritten locks, and its artifact holds them.
        if published.is_err() {
            rewritten_locks.restore(&mirror_feedback).await;
        }
        published
    }

    /// Builds the artifact of the snapshot in the staged working dir and
    /// publishes it at `destination`, the slot the build resolved.
    async fn build_and_publish(
        snapshot: &BuildSnapshot,
        ctx: &BuildContext<'_>,
        kind: ArtifactKind,
        destination: PathBuf,
    ) -> Result<PathBuf> {
        // A container build hands back the image it wrote; a process build
        // leaves its output in the working dir, which is archived whole.
        let built_image = match &snapshot.container {
            Some(container) => {
                // Container node: build the .sif via apptainer.
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
                    node_name: &snapshot.node_name,
                    node_tag: &snapshot.node_tag,
                    def_file: &container.def_file,
                    apptainer_build_extra_args,
                    lima_shell_extra_args,
                    language: snapshot.language,
                    announcer: &ctx.announcer,
                    peppy_dirs: ctx.peppy_dirs,
                    cancel_token: &ctx.cancel_token,
                })
                .await
                .map(Some)
                .map_err(|reason| snapshot.build_failed(reason))
            }
            None => {
                // Process node: run build_cmd inside the working dir.
                run_build_cmd(
                    snapshot.build_cmd.as_deref(),
                    ctx.working_dir,
                    ctx.env_vars,
                    &ctx.announcer,
                    &ctx.cancel_token,
                )
                .await
                .map(|()| None)
                .map_err(|reason| snapshot.build_failed(format!("build_cmd failed: {reason}")))
            }
        };
        let built_image = built_image?;

        // Publishing is blocking I/O (tar+zstd or fs::copy on potentially
        // multi-GB images), so it runs via `spawn_blocking` off the tokio
        // runtime worker, and before the Phase 3 write lock so the
        // parking_lot guard is never held across blocking I/O. Pruning runs
        // after the publish so storage keeps this build's artifact only.
        let working_dir = ctx.working_dir.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
            let published = match &built_image {
                Some(sif_source) => move_sif_to_storage(sif_source, &destination)?,
                None => archive_dir_to_storage(&working_dir, &destination)?,
            };
            prune_siblings(&published);
            Ok(published)
        })
        .await
        .map_err(|e| snapshot.build_failed(format!("storage publish task failed: {e}")))?
        .map_err(|e| {
            snapshot.build_failed(match kind {
                ArtifactKind::Container => {
                    format!("failed to move container image to storage: {e}")
                }
                ArtifactKind::Process => format!("failed to archive node directory: {e}"),
            })
        })
    }

    /// Undoes a start that did not reach `running`, from
    /// [`prepare_and_spawn`] (on I/O failure during the spawn/wire phases),
    /// [`abort_started`] (on caller-side ready/health failure) and
    /// [`Self::undo_abandoned_start`] (a run that panicked): a new
    /// instance is removed; a respawn's slot returns to `restarting` with no
    /// pid, or is removed when a stop path claimed it meanwhile, so no slot
    /// is `restarting` with nobody to restart it.
    ///
    /// Silent when the entity is no longer in `Ready` (a concurrent
    /// `push_config` replaced it) or the instance is not `Starting`.
    fn undo_start(handle: &Arc<RwLock<NodeEntity>>, instance_id: &Name) {
        let mut guard = handle.write();
        let NodeStage::Ready { instances, .. } = &mut guard.stage else {
            return;
        };
        let Some(pos) = instances.iter().position(|inst| {
            inst.instance_id() == instance_id && inst.state() == InstanceState::Starting
        }) else {
            return;
        };
        if instances[pos].starts_a_restart() && !instances[pos].is_stopping() {
            instances[pos].revert_respawn();
        } else {
            instances.remove(pos);
        }
    }

    /// The process group of `run`'s start in flight under `instance_id`, for
    /// the caller to kill before [`Self::undo_abandoned_start`]; `None` when
    /// the slot is not starting for `run` or its start had not forked.
    pub fn abandoned_start_group(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        run: &StartRun,
    ) -> Option<ProcessGroup> {
        let guard = handle.read();
        let pid = guard
            .instance(instance_id)
            .filter(|inst| inst.starts_for(run))?
            .pid()?;
        Some(guard.process_group(pid))
    }

    /// Undoes the start `run` left in flight under `instance_id` when it
    /// panicked, as [`Self::undo_start`] does; a slot not starting for `run`
    /// is left alone. Returns whether the slot was still `run`'s: its start
    /// in flight, now undone, or the restart it had not begun.
    pub fn undo_abandoned_start(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        run: &StartRun,
    ) -> bool {
        let (starts_for_run, awaits_its_restart) = handle
            .read()
            .instance(instance_id)
            .map_or((false, false), |inst| {
                (inst.starts_for(run), inst.awaits_restart_of(run))
            });
        if starts_for_run {
            Self::undo_start(handle, instance_id);
        }
        starts_for_run || awaits_its_restart
    }

    /// Records the spawned child's `pid` and the files of its spawn on the
    /// still-`Starting` instance `instance_id`. Called by
    /// [`Self::prepare_and_spawn`] while the entity write lock is already
    /// held, in the SAME critical section as the fork, so a daemon teardown
    /// snapshot can never observe a live child whose pid is not yet
    /// force-killable, and the daemon takes the calls of the child with the
    /// key file of its spawn from its first call on. A no-op if the entity is
    /// no longer `Ready` or the instance is gone, which cannot happen while
    /// the `Starting` instance is registered: `push_config`/`remove_config`
    /// both refuse a non-empty instances list under this same entity write
    /// lock, so the entity is neither replaced nor detached for the lifetime
    /// of the start.
    fn record_starting_process(&mut self, instance_id: &Name, pid: u32, spawn_files: SpawnFiles) {
        if let Some(inst) = self
            .instance_mut(instance_id.as_str())
            .filter(|inst| inst.state() == InstanceState::Starting)
        {
            inst.set_starting_process(pid, spawn_files);
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
    /// A [`SpawnTarget::Respawn`] registers nothing: it flips the
    /// `restarting` slot of `ctx.instance_id` to `Starting`, keeping its
    /// policy, clock, launch, copy, bindings and counts.
    ///
    /// Returns [`Error::InvalidStageTransition`] if the entity is not in
    /// `Ready` (e.g. still `Added` or currently `Building`),
    /// [`Error::DuplicateInstanceId`] if a new instance's id is already
    /// tracked in any state, [`Error::RespawnTargetMissing`] if no
    /// `restarting` slot a stop path has not claimed carries a respawn's id,
    /// or [`Error::StartClaimed`] if a stop claimed the slot before the fork.
    ///
    /// On any I/O failure inside this function, the just-registered
    /// `Starting` instance is removed, or the respawn's slot reverted,
    /// before returning.
    pub async fn prepare_and_spawn(
        handle: &Arc<RwLock<NodeEntity>>,
        ctx: StartContext<'_>,
    ) -> Result<(Child, StartedInstanceCtx)> {
        // The key file goes away on every return before the spawn hands it
        // to the started instance.
        let mut credential_guard = TempFileGuard::new(ctx.credential.to_path_buf());

        // ---- Phase 1: register the Starting instance under a brief write lock ----
        let (node_name, node_tag, node_config, artifact, start_generation) = {
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
                artifact,
                instances,
                ..
            } = &mut guard.stage
            else {
                unreachable!("ensure_spawnable just verified Ready")
            };

            let snapshot_artifact = artifact.clone();
            match &ctx.spawn {
                SpawnTarget::New(first_start) => {
                    // Reject duplicate instance ids before any I/O. Atomic
                    // with the append below; both happen under this write
                    // lock.
                    let tracked = instances
                        .iter()
                        .find(|inst| inst.instance_id() == ctx.instance_id)
                        .map(TrackedNodeInstance::state);
                    if let Some(state) = tracked {
                        return Err(Error::DuplicateInstanceId {
                            instance_id: ctx.instance_id.as_str().to_owned(),
                            node_name: guard.config.manifest.name.as_str().to_owned(),
                            node_tag: guard.config.manifest.tag.clone(),
                            core_node: ctx.core_node.to_owned(),
                            state,
                        });
                    }
                    instances.push(
                        TrackedNodeInstance::new(
                            ctx.instance_id.clone(),
                            InstanceState::Starting,
                            ctx.slot_bindings.clone(),
                        )
                        .with_clock(ctx.clock.clone())
                        .with_launch(ctx.launch.clone())
                        .with_copy(ctx.copy.clone())
                        .with_first_start(first_start.clone())
                        .with_run_log(ctx.run_log_path.to_path_buf()),
                    );
                }
                SpawnTarget::Respawn => {
                    let Some(slot) = instances.iter_mut().find(|inst| {
                        inst.instance_id() == ctx.instance_id && inst.awaits_restart()
                    }) else {
                        return Err(Error::RespawnTargetMissing {
                            instance_id: ctx.instance_id.as_str().to_owned(),
                        });
                    };
                    slot.begin_respawn();
                }
            }

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
        // A container's working directory starts empty, and so does a
        // built-in node's: it has no archive to extract.
        let instance_dir = match &artifact {
            Artifact::Built(archive) if !is_container => {
                extract_node_archive(archive, instance_id_str, ctx.peppy_dirs)
            }
            Artifact::Built(_) | Artifact::BuiltIn(_) => {
                create_instance_dir(instance_id_str, ctx.peppy_dirs)
            }
        }
        .map_err(|reason| {
            Self::undo_start(handle, ctx.instance_id);
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
            runtime_config_path: runtime_config,
            description: spawn_description,
        } = match (&artifact, node_config.execution.container.as_ref()) {
            (Artifact::BuiltIn(launch), _) => build_built_in_command(
                launch,
                &node_config,
                &instance_dir,
                ctx.runtime_config_json5,
                ctx.env_vars,
                ctx.output_sinks.announcer.log(),
                ctx.peppy_dirs,
            ),
            (Artifact::Built(sif_path), Some(container)) => {
                let apptainer_run_extra_args = container
                    .apptainer_run_extra_args
                    .as_deref()
                    .unwrap_or_default();
                let lima_shell_extra_args = container
                    .lima_shell_extra_args
                    .as_deref()
                    .unwrap_or_default();
                build_container_command(SpawnContainerInputs {
                    sif_path,
                    working_dir: &instance_dir,
                    instance_id: instance_id_str,
                    runtime_config_json5: ctx.runtime_config_json5,
                    credential: ctx.credential,
                    env_vars: ctx.env_vars,
                    mount_paths: ctx.mount_paths_resolved,
                    apptainer_run_extra_args,
                    lima_shell_extra_args,
                    announcer: &ctx.output_sinks.announcer,
                    peppy_dirs: ctx.peppy_dirs,
                })
                .await
            }
            (Artifact::Built(_), None) => build_process_command(
                &node_config,
                &instance_dir,
                ctx.runtime_config_json5,
                ctx.env_vars,
                ctx.output_sinks.announcer.log(),
                ctx.peppy_dirs,
            ),
        }
        .map_err(|e| {
            // Best-effort cleanup of the instance dir we just materialized.
            // The child never spawned, so nothing else references it; the
            // build helper already cleaned up its own runtime config temp.
            let _ = std::fs::remove_dir_all(&instance_dir);
            Self::undo_start(handle, ctx.instance_id);
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
        // A stop that claimed the slot since its flip ends the start here,
        // under the same lock as the fork, so nothing forks for it.
        let spawn_files = SpawnFiles {
            runtime_config,
            credential: ctx.credential.to_path_buf(),
        };
        let spawned = {
            let mut guard = handle.write();
            let claimed = guard
                .instance(ctx.instance_id)
                .is_some_and(TrackedNodeInstance::is_stopping);
            if claimed {
                Err(Error::StartClaimed {
                    instance_id: ctx.instance_id.as_str().to_owned(),
                })
            } else {
                command
                    .spawn()
                    .and_then(|child| {
                        // Nothing has waited on the child yet, so it has its pid.
                        let pid = child
                            .id()
                            .ok_or_else(|| std::io::Error::other("the process has no pid"))?;
                        guard.record_starting_process(ctx.instance_id, pid, spawn_files.clone());
                        Ok((child, pid))
                    })
                    .map_err(|e| Error::StartFailed {
                        node_name: node_name.clone(),
                        node_tag: node_tag.clone(),
                        reason: format!("failed to spawn `{}`: {}", spawn_description, e),
                    })
            }
        };
        let (mut child, pid) = spawned.inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&instance_dir);
            spawn_files.remove();
            Self::undo_start(handle, ctx.instance_id);
        })?;
        credential_guard.defuse();

        // ---- Phase 4: wire output streaming ----
        let stderr_buffer = StderrTail::new();
        let mut output_reader_handles = Vec::new();

        let sinks = &ctx.output_sinks;
        if let Some(stdout) = child.stdout.take() {
            output_reader_handles.push(sinks.register_and_spawn_reader(
                stdout,
                Iostream::Stdout,
                None,
            ));
        }

        if let Some(stderr) = child.stderr.take() {
            output_reader_handles.push(sinks.register_and_spawn_reader(
                stderr,
                Iostream::Stderr,
                Some(stderr_buffer.clone()),
            ));
        }

        Ok((
            child,
            StartedInstanceCtx {
                instance_dir,
                spawn_files,
                stderr_buffer,
                output_reader_handles,
                group: ProcessGroup { pid, is_container },
                generation: start_generation,
            },
        ))
    }

    /// Phase 2 (success): records the spawned instance against the entity and
    /// transitions `Starting → Running`. Returns the live `Child` so the caller
    /// can watch it for exit (see the process-exit watcher in `node_run`),
    /// which moves a self-exit to `Restarting`, `Finished` or `Failed` by the
    /// instance's policy.
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
    /// exit, while the stop paths and the stack's drop still drive termination
    /// by the pid `prepare_and_spawn` recorded at the fork.
    ///
    /// `endpoints` is what the start sequence read from the node and expanded
    /// once it was healthy; the commit records it on the instance, the one
    /// place the set is written. A respawn's commit counts the restart. The
    /// run of the process starts at `now`.
    pub async fn commit_started(
        handle: &Arc<RwLock<NodeEntity>>,
        mut child: Child,
        started_ctx: StartedInstanceCtx,
        instance_id: Name,
        endpoints: Vec<InstanceEndpoint>,
        now: Instant,
    ) -> Result<Child> {
        // Helper: every error path kills the start's process group, its
        // forked children and a container's in-VM group included, before the
        // leader is reaped, so the group's id is still its own; tokio `Child`
        // has no kill_on_drop set in the spawn helpers.
        async fn kill_start(child: &mut Child, group: ProcessGroup, instance_id: &str) {
            group.force_kill(instance_id).await;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // Take the on-disk paths out of `started_ctx` so we can both persist
        // them onto the running instance (success) and clean them up
        // (failure) without partial moves.
        let StartedInstanceCtx {
            instance_dir,
            spawn_files,
            generation: start_generation,
            group,
            ..
        } = started_ctx;

        // The write guard is a temporary of this statement, so it is released
        // before the cleanup below awaits.
        let committed = handle.write().commit_starting_instance(
            start_generation,
            &instance_id,
            &instance_dir,
            endpoints,
            now,
        );
        if let Err(error) = committed {
            // Concurrent push_config / stale generation / inconsistent
            // state: the entity is no longer ours, but the spawned
            // process and the on-disk artifacts created during this
            // start *are*; kill the child and clean up the temp files
            // before returning so nothing orphans.
            kill_start(&mut child, group, instance_id.as_str()).await;
            let _ = std::fs::remove_dir_all(&instance_dir);
            spawn_files.remove();
            return Err(error);
        }

        // Successful commit: hand the live Child back without killing.
        // The OS process keeps running; the caller moves it into the
        // exit watcher, which observes the eventual exit (clean or
        // crash) and reaps the process.
        Ok(child)
    }

    /// The entity half of [`Self::commit_started`]: flips the `Starting`
    /// instance `instance_id` to `Running`, records its on-disk paths and
    /// endpoints and its supervision's start of a run at `now`. Refuses, and leaves
    /// the entity untouched, when this is not the entity the start
    /// registered on (generation `start_generation`), the entity is not
    /// `Ready`, or the instance is missing or not `Starting`.
    fn commit_starting_instance(
        &mut self,
        start_generation: u64,
        instance_id: &Name,
        instance_dir: &Path,
        endpoints: Vec<InstanceEndpoint>,
        now: Instant,
    ) -> Result<()> {
        let refuse = |from: &'static str, to: &'static str| Error::InvalidStageTransition {
            node_name: self.config.manifest.name.as_str().to_owned(),
            node_tag: self.config.manifest.tag.clone(),
            from,
            to,
        };
        // Generation guard: if a concurrent `push_config` replaced the entity
        // wholesale, the new entity has a different generation and is *not*
        // ours to mutate, so its instances list stays untouched.
        if self.generation != start_generation {
            return Err(refuse("stale-generation", "Running"));
        }
        let NodeStage::Ready { instances, .. } = &mut self.stage else {
            return Err(refuse(self.stage.name(), "Ready"));
        };
        let Some(instance) = instances
            .iter_mut()
            .find(|instance| instance.instance_id() == instance_id)
        else {
            return Err(refuse("missing", "Running"));
        };
        if instance.state() != InstanceState::Starting {
            return Err(refuse("Running", "Running"));
        }
        instance.set_running(instance_dir.to_path_buf());
        instance.set_endpoints(endpoints);
        instance.record_running(now);
        Ok(())
    }

    /// Phase 2 (failure): kills the spawned child's group, joins the reader
    /// tasks (so the stderr buffer flushes), undoes the start (a new
    /// instance's `Starting` slot is removed, a respawn's goes back to
    /// `Restarting`, or is removed when a stop path claimed it, see
    /// `undo_start`), and returns a formatted error message
    /// including a stderr tail.
    ///
    /// If a concurrent `push_config` replaced the entity wholesale while the
    /// daemon was running its messenger checks, the instance removal is
    /// silently skipped; the new state takes precedence. The child is still
    /// killed either way.
    ///
    /// The caller decides how the instance ends before this: a start given up
    /// after the ready signal has stopped it through its shutdown request
    /// already, and the kill here only reaps what is left. The output readers
    /// are joined on `time`.
    pub async fn abort_started(
        handle: &Arc<RwLock<NodeEntity>>,
        child: Child,
        started_ctx: StartedInstanceCtx,
        error: String,
        instance_id: &Name,
        time: &TimeSource,
    ) -> String {
        let StartedInstanceCtx {
            instance_dir,
            spawn_files,
            stderr_buffer,
            output_reader_handles,
            group,
            generation: start_generation,
        } = started_ctx;

        let msg = kill_and_collect_error(
            child,
            group,
            instance_id.as_str(),
            &error,
            stderr_buffer,
            output_reader_handles,
            time,
        )
        .await;

        // Best-effort cleanup of the on-disk instance directory and the
        // files of the spawn. Once the child has been killed and the readers
        // drained, nothing else holds file descriptors into them.
        let _ = std::fs::remove_dir_all(&instance_dir);
        spawn_files.remove();

        // Generation guard: only touch the entity's instances list if it is
        // still the same entity we registered against. A concurrent
        // `push_config` would have bumped the generation, in which case the
        // replacement entity owns its own instances and we must not poke at
        // them.
        if handle.read().generation == start_generation {
            Self::undo_start(handle, instance_id);
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
                instance: Box::new(instance),
            },
            Vec::new(),
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
                artifact: Artifact::Built(artifact_path),
                instances,
            },
        };
        Self::with_stage(config, stage, Vec::new())
    }

    /// Records on the slot that the process of a `Running` instance exited
    /// on its own: the exit, the run's length and the attempt its policy
    /// takes next. The instance stays in the entity so it remains visible
    /// in `stack list`.
    ///
    /// `None` if the instance is missing, not `Running`, runs a process other
    /// than `pid` (a new instance under the id since a stop removed this one),
    /// marked `stopping` (a stop path owns its removal, so an intentional kill
    /// is never shown as a crash), or the entity is not `Ready`. The
    /// `stopping` check and the state flip happen together under the write
    /// lock, so they cannot race a concurrent stop. `take_registrations` runs
    /// under the same write lock: it takes the registrations the exit ends (the
    /// instance's observer records and watchers), so an edit of them that
    /// lands after the flip finds the record on the slot. The process exited
    /// at `now`.
    pub fn mark_instance_exited(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        pid: u32,
        cause: ExitCause,
        backoff: &RestartBackoff,
        now: Instant,
        take_registrations: impl FnOnce() -> Registrations,
    ) -> Option<ExitRecord> {
        let mut guard = handle.write();
        let inst = guard
            .instance_mut(instance_id.as_str())
            .filter(|inst| inst.runs_unclaimed_process(pid))?;
        Some(inst.record_exit(cause, backoff, now, take_registrations))
    }

    /// Records the failure at `now` of a `Restarting` instance's start that
    /// did not reach `running`, and takes the backoff's next attempt. `None`
    /// when the instance is not `restarting`, or a stop path claimed it.
    pub fn mark_start_failed(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        exit: ExitCause,
        backoff: &RestartBackoff,
        now: Instant,
    ) -> Option<Attempt> {
        Self::edit_awaiting_restart(handle, instance_id, |inst| {
            inst.supervision.last_exit = Some(exit);
            inst.take_next_attempt(backoff, None, now)
        })
    }

    /// Ends the restarts of a `Restarting` instance the daemon refused to
    /// start again: it is `Failed` from here. `false` when the instance is
    /// not `restarting`, or a stop path claimed it.
    pub fn mark_restart_refused(handle: &Arc<RwLock<NodeEntity>>, instance_id: &Name) -> bool {
        Self::edit_awaiting_restart(handle, instance_id, |inst| {
            inst.state = InstanceState::Failed;
            inst.supervision.next_start = None;
        })
        .is_some()
    }

    /// Records the run log a restart's start opened on the slot that awaits
    /// it, before the start begins, so `peppy node info` names the file of a
    /// start that fails before its spawn and the next attempt's opening line
    /// names it as the previous log. A slot a stop path claimed records
    /// nothing: its start finds no slot to flip.
    pub fn record_respawn_log(handle: &Arc<RwLock<NodeEntity>>, instance_id: &Name, path: PathBuf) {
        Self::edit_awaiting_restart(handle, instance_id, |inst| inst.run_log_path = Some(path));
    }

    /// Runs `edit` on the slot of an instance that awaits its restart
    /// (`Restarting`, and no stop path claimed it), under the write lock.
    /// `None` when no such slot has the id.
    fn edit_awaiting_restart<T>(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        edit: impl FnOnce(&mut TrackedNodeInstance) -> T,
    ) -> Option<T> {
        handle
            .write()
            .instance_mut(instance_id.as_str())
            .filter(|inst| inst.awaits_restart())
            .map(edit)
    }

    /// Claims and removes an instance in one write-lock section, so its exit
    /// watcher finds no slot and a restart it awaits is cancelled: a `Running`
    /// instance only while it runs `stopped_pid`, the process the caller
    /// stopped or none, and one without a process (`Restarting`, `Finished`,
    /// `Failed`) whatever `stopped_pid` is. A `Starting` instance, or one
    /// running another process (one a start committed meanwhile), stays.
    pub fn claim_and_remove(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        stopped_pid: Option<u32>,
    ) -> SlotRemoval {
        let mut guard = handle.write();
        let Some(inst) = guard.instance(instance_id) else {
            return SlotRemoval::Untracked;
        };
        match inst.state() {
            InstanceState::Starting => {
                return SlotRemoval::Left(SlotLeft::Starting {
                    restart: inst.starts_a_restart(),
                });
            }
            InstanceState::Running if inst.pid() != stopped_pid => {
                return SlotRemoval::Left(SlotLeft::RunsAnotherProcess);
            }
            InstanceState::Running
            | InstanceState::Restarting
            | InstanceState::Finished
            | InstanceState::Failed => {}
        }
        guard
            .claim_and_remove_slot(instance_id)
            .map_or(SlotRemoval::Untracked, SlotRemoval::Removed)
    }

    /// Claims the slot of `instance_id` and removes it, under the write lock
    /// the caller holds; `None` when no slot was removed.
    fn claim_and_remove_slot(&mut self, instance_id: &Name) -> Option<KeptRelations> {
        self.claim_instance(instance_id)?;
        self.stop_instance(instance_id)
    }

    /// Whether `instance_id` runs the process `pid` with no stop path's
    /// claim on it: that process's exit is its exit watcher's to act on.
    pub fn runs_unclaimed_process(&self, instance_id: &Name, pid: u32) -> bool {
        self.instance(instance_id)
            .is_some_and(|inst| inst.runs_unclaimed_process(pid))
    }

    /// Claims the instance for a stop path, under the entity's read lock the
    /// caller holds: its exit watcher leaves the slot to that path and a
    /// restart it awaits is cancelled, see
    /// [`TrackedNodeInstance::mark_stopping`]. Returns the slot.
    pub fn claim_instance(&self, instance_id: &Name) -> Option<&TrackedNodeInstance> {
        let inst = self.instance(instance_id)?;
        inst.mark_stopping();
        Some(inst)
    }

    /// Claims the instance for a stop path, as [`Self::claim_instance`]
    /// does, when it still runs the process `pid` with no claim on it; one
    /// another stop path claimed, or that runs another process or none,
    /// stays as it is.
    pub fn claim_process(&self, instance_id: &Name, pid: u32) -> ProcessClaim {
        let Some(inst) = self
            .instance(instance_id)
            .filter(|inst| inst.runs_process(pid))
        else {
            return ProcessClaim::Gone;
        };
        if !inst.claim_once() {
            return ProcessClaim::ClaimedElsewhere;
        }
        ProcessClaim::Claimed
    }

    /// Claims an instance whose restart is starting its process, which
    /// cancels that start through the restart token, and returns what its
    /// slot kept of its relations; any other start, or another state, is
    /// named and left as it is.
    pub fn claim_starting(&self, instance_id: &Name) -> StartingClaim {
        let Some(inst) = self
            .instance(instance_id)
            .filter(|inst| inst.state() == InstanceState::Starting)
        else {
            return StartingClaim::NotStarting;
        };
        if !inst.is_stopping() && !inst.starts_a_restart() {
            return StartingClaim::FirstStart;
        }
        if !inst.claim_once() {
            return StartingClaim::ClaimedElsewhere;
        }
        StartingClaim::Claimed(KeptRelations::of(&inst.supervision))
    }

    /// The slot of `instance_id` while it awaits its restart.
    fn awaiting_restart_slot(&self, instance_id: &Name) -> Option<&TrackedNodeInstance> {
        self.instance(instance_id)
            .filter(|inst| inst.awaits_restart())
    }

    /// What a respawn of `instance_id` starts from: the goal of its first
    /// start with what the slot holds now, and the pairs it held. `None` when
    /// the instance does not await its restart.
    pub fn respawn_inputs_of(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
    ) -> Option<RespawnInputs> {
        let guard = handle.read();
        let inst = guard.awaiting_restart_slot(instance_id)?;
        // A slot without a first start has the policy `never`, so it never
        // waits for a restart.
        let first_start = inst.supervision.first_start.as_ref()?;
        let Registrations {
            observations,
            watchers,
        } = inst.supervision.registrations.clone();
        Some(RespawnInputs {
            goal: first_start
                .goal
                .for_respawn(inst.slot_bindings.clone(), observations, watchers),
            held_pairs: inst.supervision.held_pairs.clone(),
            restart: first_start.restart,
            first_start_goal: Arc::clone(&first_start.goal),
            restart_count: inst.supervision.restart_count,
            previous_run_log: inst.run_log_path.clone(),
        })
    }

    /// Records a pair delivered to the instance of `pair.own`, once, so its
    /// next process requests it again, whatever the instance's state. A pair
    /// on a slot the manifest declares to take one peer replaces the pairs
    /// that slot held, which come back as displaced. `None` when the entity
    /// tracks no such instance or its node declares no such pairing slot.
    pub(crate) fn record_held_pair(&mut self, pair: &PlannedPair) -> Option<Vec<PlannedPair>> {
        let own_cardinality = self.pairing_slot_cardinality(&pair.own.link_id)?;
        let inst = self.instance_mut(pair.own.instance_id.as_str())?;
        let held = &mut inst.supervision.held_pairs;
        let displaced = if own_cardinality.is_scalar() {
            let (displaced, kept) = std::mem::take(held)
                .into_iter()
                .partition(|other| other.own == pair.own && other != pair);
            *held = kept;
            displaced
        } else {
            Vec::new()
        };
        if !held.contains(pair) {
            held.push(pair.clone());
        }
        Some(displaced)
    }

    /// How many peers this node's pairing slot `link_id` takes, from its
    /// manifest; `None` when the node declares no such slot.
    fn pairing_slot_cardinality(&self, link_id: &str) -> Option<Cardinality> {
        self.config()
            .manifest
            .depends_on
            .as_ref()?
            .pairings
            .iter()
            .find(|participant| participant.link_id == link_id)
            .map(|participant| participant.cardinality)
    }

    /// Forgets a pair the instance of `pair.own` held.
    pub(crate) fn forget_held_pair(&mut self, pair: &PlannedPair) {
        self.edit_instance(&pair.own.instance_id, |inst| {
            inst.supervision.held_pairs.retain(|held| held != pair);
        });
    }

    /// Forgets, on every instance of this entity, the pairs held with the
    /// instance `peer_instance_id` of the daemon `peer_core_node`; an
    /// instance of the same id on another daemon is another instance and
    /// stays.
    pub(crate) fn forget_pairs_held_with(&mut self, peer_core_node: &str, peer_instance_id: &str) {
        for inst in self.instances_mut() {
            inst.supervision.held_pairs.retain(|held| {
                held.peer.core_node != peer_core_node || held.peer.instance_id != peer_instance_id
            });
        }
    }

    /// The slots of a `Ready` entity, for an edit.
    fn instances_mut(&mut self) -> impl Iterator<Item = &mut TrackedNodeInstance> {
        let instances = match &mut self.stage {
            NodeStage::Ready { instances, .. } => Some(instances),
            _ => None,
        };
        instances
            .into_iter()
            .flat_map(|instances| instances.iter_mut())
    }

    /// The slot of `instance_id`, in any state.
    pub fn instance(&self, instance_id: &Name) -> Option<&TrackedNodeInstance> {
        self.instances()
            .iter()
            .find(|inst| inst.instance_id() == instance_id)
    }

    /// The slot of `instance_id`, in any state, for an edit.
    fn instance_mut(&mut self, instance_id: &str) -> Option<&mut TrackedNodeInstance> {
        self.instances_mut()
            .find(|inst| inst.instance_id().as_str() == instance_id)
    }

    /// Runs `edit` on the slot of the `Restarting` instance `instance_id`,
    /// claimed or not, for an edit of what its next process starts from (a
    /// stop that claimed it removes the slot next). `false` when no
    /// `Restarting` instance has the id.
    fn edit_restarting(
        &mut self,
        instance_id: &str,
        edit: impl FnOnce(&mut TrackedNodeInstance),
    ) -> bool {
        self.instance_mut(instance_id)
            .filter(|inst| inst.state() == InstanceState::Restarting)
            .map(edit)
            .is_some()
    }

    /// The process group `pid` leads in one of this node's instances.
    fn process_group(&self, pid: u32) -> ProcessGroup {
        ProcessGroup {
            pid,
            is_container: self.config.execution.container.is_some(),
        }
    }

    /// Upserts the member sets of the named observer slots a `Restarting`
    /// instance observes again at its restart; slots the map does not name
    /// keep theirs. `false` when no `Restarting` instance has the id.
    pub(crate) fn record_observations_for_restart(
        &mut self,
        instance_id: &str,
        observations: &BTreeMap<String, ObservationTargets>,
    ) -> bool {
        self.edit_restarting(instance_id, |inst| {
            inst.supervision.registrations.observations.extend(
                observations
                    .iter()
                    .map(|(observer_link_id, targets)| (observer_link_id.clone(), targets.clone())),
            );
        })
    }

    /// Replaces the lifecycle watchers a `Restarting` instance announces to
    /// at its restart, as a join or removal rewrites them; a running
    /// instance's are the registry's.
    pub(crate) fn record_watchers_for_restart(&mut self, instance_id: &str, watchers: &[String]) {
        self.edit_restarting(instance_id, |inst| {
            inst.supervision.registrations.watchers = watchers.to_vec();
        });
    }

    /// Arms the health monitor's `kill` of the `Running` instance
    /// `instance_id`, so its exit counts as a failure, and returns the group
    /// to signal; `None` when the instance is not `Running`, a stop path
    /// claimed it, or it has no pid.
    pub(crate) fn arm_health_kill(
        &mut self,
        instance_id: &str,
        kill: HealthKill,
    ) -> Option<ProcessGroup> {
        let inst = self
            .instance_mut(instance_id)
            .filter(|inst| inst.runs_unclaimed())?;
        let pid = inst.pid()?;
        inst.supervision.health_kill = Some(kill);
        Some(self.process_group(pid))
    }

    /// Runs `edit` on the slot of `instance_id`, in any state. `false` when
    /// the entity is not `Ready` or tracks no such instance.
    fn edit_instance(
        &mut self,
        instance_id: &str,
        edit: impl FnOnce(&mut TrackedNodeInstance),
    ) -> bool {
        self.instance_mut(instance_id).map(edit).is_some()
    }

    /// Notes that a removal of this node stops the process `pid` of
    /// `instance_id`, by a shutdown request or, for one its probe found
    /// unreachable, by its force phase, while the instance runs it: while
    /// any removal's note stands, the instance's exit ends it whatever its
    /// policy.
    pub fn note_shutdown_request(&mut self, instance_id: &Name, pid: u32) {
        self.edit_process(instance_id, pid, |inst| inst.shutdown_requests += 1);
    }

    /// Withdraws one removal's note of [`Self::note_shutdown_request`] from
    /// an instance it leaves, while the instance still runs the process `pid`
    /// the note was for; once no removal's note stands, its policy applies
    /// again. An instance that exited under the note, or another instance
    /// under the id since, keeps its count.
    pub fn withdraw_shutdown_request(&mut self, instance_id: &Name, pid: u32) {
        self.edit_process(instance_id, pid, |inst| {
            inst.shutdown_requests = inst.shutdown_requests.saturating_sub(1);
        });
    }

    /// Runs `edit` on the slot of `instance_id` while it runs the process
    /// `pid`.
    fn edit_process(
        &mut self,
        instance_id: &Name,
        pid: u32,
        edit: impl FnOnce(&mut TrackedNodeInstance),
    ) {
        if let Some(inst) = self
            .instance_mut(instance_id.as_str())
            .filter(|inst| inst.runs_process(pid))
        {
            edit(inst);
        }
    }

    /// Records the producer set one of this entity's instances holds in slot
    /// `link_id` from now on. Returns `false` when the entity is not `Ready` or
    /// tracks no such instance.
    pub fn set_instance_slot_binding(
        &mut self,
        instance_id: &Name,
        link_id: &str,
        producers: config::runtime::BoundProducers,
    ) -> bool {
        self.edit_instance(instance_id.as_str(), |inst| {
            inst.set_slot_binding(link_id, producers)
        })
    }

    /// Records the producer set a `Restarting` instance takes in slot
    /// `link_id` at its restart. `false` when no `Restarting` instance has
    /// the id.
    pub(crate) fn record_slot_binding_for_restart(
        &mut self,
        instance_id: &Name,
        link_id: &str,
        producers: config::runtime::BoundProducers,
    ) -> bool {
        self.edit_restarting(instance_id.as_str(), |inst| {
            inst.set_slot_binding(link_id, producers)
        })
    }

    /// Removes a `Running`, `Restarting` or terminal (`Finished`/`Failed`)
    /// instance from a `Ready` entity. The entity stays in `Ready` regardless
    /// of whether the instance list becomes empty. `Starting` instances are intentionally left
    /// alone; to clean those up, the caller must use `abort_started`.
    ///
    /// Terminal instances are removable here so an explicit stop (or a stack
    /// clear) can clear out a one-shot node that already exited on its own, and
    /// so a self-exit that raced the stop path's removal cannot leave an
    /// untracked instance behind.
    ///
    /// Returns what the removed slot kept of the instance's relations, for
    /// the announcement of its end; `None` when nothing was removed (the
    /// instance is missing or `Starting`, or the entity is not `Ready`).
    pub fn stop_instance(&mut self, instance_id: &Name) -> Option<KeptRelations> {
        let NodeStage::Ready { instances, .. } = &mut self.stage else {
            return None;
        };
        let pos = instances.iter().position(|inst| {
            inst.instance_id() == instance_id && inst.state() != InstanceState::Starting
        })?;
        let removed = instances.remove(pos);
        remove_instance_files(&removed);
        Some(KeptRelations::of(&removed.supervision))
    }
}

/// Best-effort cleanup of the on-disk artifacts the start path recorded on
/// a removed instance. Errors are ignored so a removed file or a missing
/// directory does not block the lifecycle transition.
fn remove_instance_files(removed: &TrackedNodeInstance) {
    if let Some(dir) = removed.instance_dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Some(files) = &removed.spawn_files {
        files.remove();
    }
}

#[derive(Debug, Clone)]
pub struct TrackedNodeInstance {
    instance_id: Name,
    /// Process id of the child the spawn path recorded for this instance, the
    /// leader of its process group, so the stop paths and the stack's own drop
    /// can signal it. Only `prepare_and_spawn` records one, under the entity
    /// lock, so every pid here names a child of this daemon. `None` for the
    /// root instance (the daemon's own process), for instances running on
    /// remote locations (e.g., embedded systems) where a local pid is not
    /// available, and once the process has exited on its own.
    pid: Option<u32>,
    state: InstanceState,
    /// The incarnation of the process this instance runs, is starting or last
    /// ran: drawn at the first start and again at each restart's
    /// [`Self::begin_respawn`], so a process started under a reused id never
    /// carries an earlier process's.
    incarnation: u64,
    /// On-disk instance directory created during start (extracted archive
    /// or freshly-created container instance dir). Persisted on the
    /// `Running` instance so `stop_instance` can clean it up. `None` for
    /// snapshot-restored or test-fixture instances.
    instance_dir: Option<PathBuf>,
    /// The runtime config temp file created by `write_runtime_config_temp`
    /// and the key file of the process. Persisted so they can be removed
    /// when the instance stops or aborts. `None` for snapshot-restored or
    /// test-fixture instances.
    spawn_files: Option<SpawnFiles>,
    /// The run log of the latest start, `<instance_id>_<timestamp>.log`: the
    /// current or last process's, or a restart attempt's that failed before
    /// its spawn. `peppy node info` reports it. `None` for the core node's own
    /// instance and a snapshot-restored one.
    run_log_path: Option<PathBuf>,
    /// Producers bound to each of this consumer instance's `depends_on`
    /// slots, mirroring
    /// [`config::runtime::NodeInstanceConfig::slot_bindings`].
    /// Surfaced through `node_info` so the launcher / CLI can
    /// cross-check newly-staged binding plans against running
    /// consumers' existing claims. Empty when the node has no
    /// `depends_on` slots.
    slot_bindings: config::runtime::SlotBindings,
    /// The one clock this instance reads for its lifetime, as the change that
    /// started it resolved. Surfaced through `stack list` and `node_info`, so
    /// a connection staged against a running instance is judged against the
    /// clock it actually reads.
    clock: config::runtime::ClockBinding,
    /// The launch this instance belongs to, as the change that started it
    /// named. `clock_list` reads it to say which launch owns a domain this
    /// instance publishes. `None` for an instance `peppy node run` started,
    /// and for a snapshot-restored or test-fixture instance.
    launch: Option<LaunchIdentity>,
    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run on its own.
    copy: Option<config::runtime::Name>,
    /// Last `node_health` outcome recorded by the daemon's health monitor.
    /// Behind an `Arc<AtomicBool>` so the monitor can update it through the
    /// cheap clone returned by `NodeStack::find_by_instance_id`, without taking
    /// an entity write lock. `true` until a probe is observed to fail; surfaced
    /// by `stack list` so it reports health without a per-instance round-trip.
    healthy: Arc<AtomicBool>,
    /// Set by the stop paths before they terminate this instance's process, so
    /// the process-exit watcher can tell a daemon-initiated stop (cooperative
    /// shutdown or force-kill) apart from a self-exit. When set, the watcher
    /// leaves the state alone and lets the stop path own removal: the exit is
    /// not recorded, so an intentional force-kill is neither a crash nor a
    /// reason to restart. An atomic, so a claim flips it through `&self`
    /// under a read or a write lock on the entity, behind an `Arc` that every
    /// clone of the slot shares.
    stopping: Arc<AtomicBool>,
    /// The removals stopping this instance's process: while any is, its exit
    /// ends the instance whatever its policy, as the removal removes the slot
    /// next.
    shutdown_requests: u32,
    /// The endpoints the instance serves, as the daemon expanded the sockets
    /// it announced at start; empty for a node whose manifest declares none,
    /// until the instance commits to `Running` and once its process exits.
    endpoints: Vec<InstanceEndpoint>,
    /// What the stack keeps of the instance across its processes.
    supervision: Supervision,
}

impl TrackedNodeInstance {
    /// Constructs a new tracked instance. The `state` must be supplied
    /// explicitly; there is no default. Callers that have just spawned a
    /// child process and have not yet committed it pass `InstanceState::Starting`;
    /// callers that are reconstructing an entity from a snapshot or test
    /// fixture pass `InstanceState::Running`. `slot_bindings` carries the
    /// validator-resolved per-slot bindings for this instance; pass an
    /// empty map when reconstructing test fixtures or instances whose
    /// manifest has no `depends_on` slots. A new instance carries no pid:
    /// the spawn path records the child's pid once it has forked it, so a
    /// pid can never be made up for an instance and signaled later.
    pub fn new(
        instance_id: Name,
        state: InstanceState,
        slot_bindings: config::runtime::SlotBindings,
    ) -> Self {
        Self {
            instance_id,
            pid: None,
            state,
            incarnation: next_incarnation(),
            instance_dir: None,
            spawn_files: None,
            run_log_path: None,
            slot_bindings,
            clock: config::runtime::ClockBinding::Wall,
            launch: None,
            copy: None,
            healthy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            stopping: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown_requests: 0,
            endpoints: Vec::new(),
            supervision: Supervision::default(),
        }
    }

    /// Records the policy and the goal of the instance's first start.
    fn with_first_start(mut self, first_start: FirstStart) -> Self {
        self.supervision.first_start = Some(first_start);
        self
    }

    /// Whether the instance is `Restarting` and no stop path claimed it.
    fn awaits_restart(&self) -> bool {
        self.state == InstanceState::Restarting && !self.is_stopping()
    }

    /// Whether this instance runs a process no stop path has claimed: the
    /// one its exit and the health monitor's kill act on.
    fn runs_unclaimed(&self) -> bool {
        self.state == InstanceState::Running && !self.is_stopping()
    }

    /// Whether this instance keeps a held pair from its slot `own` with
    /// `peer`, which its restarts ask for again.
    pub(crate) fn keeps_held_pair(&self, own: &SlotAddr, peer: &SlotAddr) -> bool {
        self.supervision
            .held_pairs
            .iter()
            .any(|held| &held.own == own && &held.peer == peer)
    }

    /// Whether this instance runs the process `pid`, claimed or not.
    pub(crate) fn runs_process(&self, pid: u32) -> bool {
        self.state == InstanceState::Running && self.pid == Some(pid)
    }

    /// Whether this instance runs the process `pid`, unclaimed.
    fn runs_unclaimed_process(&self, pid: u32) -> bool {
        self.runs_process(pid) && !self.is_stopping()
    }

    /// Whether this instance is starting the process of a restart: an exit
    /// took its first attempt, which a first start has not.
    pub fn starts_a_restart(&self) -> bool {
        self.state == InstanceState::Starting && self.supervision.attempt > 0
    }

    /// Whether this instance was first started from `goal` (`Arc::ptr_eq`),
    /// which tells it from a later instance under the id.
    fn first_started_from(&self, goal: &Arc<NodeRunGoal>) -> bool {
        self.supervision
            .first_start
            .as_ref()
            .is_some_and(|first| Arc::ptr_eq(&first.goal, goal))
    }

    /// Whether this is the instance first started from `goal`, at the
    /// restart count `restart_count`.
    fn is_restart_of(&self, goal: &Arc<NodeRunGoal>, restart_count: u32) -> bool {
        self.first_started_from(goal) && self.supervision.restart_count == restart_count
    }

    /// Whether the start in flight here is `run`'s: a first start from
    /// `run`'s goal, or a restart's start begun at the restart count `run`
    /// names by the instance first started from `run`'s goal.
    fn starts_for(&self, run: &StartRun) -> bool {
        match run {
            StartRun::New(goal) => {
                self.state == InstanceState::Starting
                    && !self.starts_a_restart()
                    && self.first_started_from(goal)
            }
            StartRun::Respawn {
                goal,
                restart_count,
            } => self.starts_a_restart() && self.is_restart_of(goal, *restart_count),
        }
    }

    /// Whether this instance still awaits the restart `run` was to start:
    /// the run ended before its start began.
    fn awaits_restart_of(&self, run: &StartRun) -> bool {
        match run {
            StartRun::New(_) => false,
            StartRun::Respawn {
                goal,
                restart_count,
            } => self.awaits_restart() && self.is_restart_of(goal, *restart_count),
        }
    }

    /// The instance's restart policy and its restarts so far, as the wire
    /// carries them.
    pub fn restart_status(&self) -> RestartStatus {
        self.supervision.status()
    }

    /// What the stack keeps of this instance across its processes.
    #[cfg(any(test, feature = "test-support"))]
    pub fn supervision(&self) -> &Supervision {
        &self.supervision
    }

    /// Whether a removal stops this instance's process, so that its exit
    /// ends the instance whatever its policy.
    #[cfg(any(test, feature = "test-support"))]
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requests > 0
    }

    /// Returns the validator-resolved per-slot bindings recorded for
    /// this instance. Empty for instances whose manifest has no
    /// `depends_on` slots or for snapshot-restored / test-fixture
    /// instances built with an empty bindings map.
    pub fn slot_bindings(&self) -> &config::runtime::SlotBindings {
        &self.slot_bindings
    }

    /// The one clock this instance reads. Wall time for an instance whose
    /// deployment named no domain, which is also what a snapshot-restored or
    /// test-fixture instance carries.
    pub fn clock(&self) -> &config::runtime::ClockBinding {
        &self.clock
    }

    /// Records the clock the change that started this instance resolved.
    pub(crate) fn with_clock(mut self, clock: config::runtime::ClockBinding) -> Self {
        self.clock = clock;
        self
    }

    /// The launch this instance belongs to. `None` for an instance
    /// `peppy node run` started, which is also what a snapshot-restored or
    /// test-fixture instance carries.
    pub fn launch(&self) -> Option<&LaunchIdentity> {
        self.launch.as_ref()
    }

    /// Records the launch the change that started this instance belongs to.
    pub(crate) fn with_launch(mut self, launch: Option<LaunchIdentity>) -> Self {
        self.launch = launch;
        self
    }

    /// Records the run log of the process this start makes.
    pub fn with_run_log(mut self, path: PathBuf) -> Self {
        self.run_log_path = Some(path);
        self
    }

    /// The copy this instance belongs to; `None` for an instance run on its
    /// own.
    pub fn copy(&self) -> Option<&config::runtime::Name> {
        self.copy.as_ref()
    }

    /// Records the copy this instance belongs to.
    pub(crate) fn with_copy(mut self, copy: Option<config::runtime::Name>) -> Self {
        self.copy = copy;
        self
    }

    /// Replaces the producers recorded for one of this instance's slots with
    /// the set the daemon delivered to it.
    fn set_slot_binding(&mut self, link_id: &str, producers: config::runtime::BoundProducers) {
        self.slot_bindings.insert(link_id.to_string(), producers);
    }

    /// The endpoints the instance serves, in label order, for `stack list`
    /// and `node info`; empty for a node whose manifest declares none.
    pub fn endpoints(&self) -> &[InstanceEndpoint] {
        &self.endpoints
    }

    /// Same-module mutator used by `NodeEntity::commit_started` to record the
    /// endpoints the start sequence read from the node and expanded, the one
    /// writer of the set. Not exported.
    fn set_endpoints(&mut self, endpoints: Vec<InstanceEndpoint>) {
        self.endpoints = endpoints;
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

    /// The incarnation of the process this instance runs, or starts once the
    /// spawn has forked it (its pid is recorded); `None` before the fork and
    /// once the process is gone.
    pub(crate) fn live_incarnation(&self) -> Option<u64> {
        let forked = self.state == InstanceState::Running || self.pid.is_some();
        (self.state.has_process() && forked).then_some(self.incarnation)
    }

    /// The last `node_health` outcome the health monitor recorded for this
    /// instance. `true` until a probe is observed to fail.
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Records the latest `node_health` outcome. Takes `&self` because the flag
    /// is an `Arc<AtomicBool>`; the health monitor updates it through the clone
    /// returned by `NodeStack::find_by_instance_id`.
    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    /// `true` once a stop path has claimed this instance for termination
    /// (see [`Self::mark_stopping`]): its exit is not recorded, and no restart
    /// starts it.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// Claims this instance for a deliberate stop: sets its stopping flag and
    /// cancels a restart it waits for. Idempotent. Takes `&self` because the
    /// flag is an `Arc<AtomicBool>`: a claim flips it under a lock on the
    /// entity, which excludes the write lock `mark_instance_exited` holds
    /// across its `is_stopping` check and its state flip, so a self-exit
    /// racing the claim is never recorded.
    pub fn mark_stopping(&self) {
        let _ = self.claim_once();
    }

    /// Sets the stopping flag, before the instance's process is signaled, and
    /// cancels a restart the instance waits for; `true` when this call set
    /// the flag, so of two calls at once one returns `true`.
    #[must_use]
    fn claim_once(&self) -> bool {
        let claimed = !self
            .stopping
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        self.supervision.restart_token.cancel();
        claimed
    }

    /// Returns the on-disk instance directory recorded during start, if any.
    pub fn instance_dir(&self) -> Option<&Path> {
        self.instance_dir.as_deref()
    }

    /// Returns the on-disk runtime config temp file recorded during start,
    /// if any.
    pub fn runtime_config_path(&self) -> Option<&Path> {
        self.spawn_files
            .as_ref()
            .map(|files| files.runtime_config.as_path())
    }

    /// Returns the key file of the process recorded during start, if any.
    pub fn credential_path(&self) -> Option<&Path> {
        self.spawn_files
            .as_ref()
            .map(|files| files.credential.as_path())
    }

    /// The run log of the latest start (the current or last process's, or a
    /// restart attempt's that failed before its spawn), when one recorded it.
    pub fn run_log_path(&self) -> Option<&Path> {
        self.run_log_path.as_deref()
    }

    /// Same-module mutator used by `NodeEntity::commit_started` to flip a
    /// `Starting` instance to `Running` and record the instance directory
    /// produced by `prepare_and_spawn`. The pid and the spawn files stay as
    /// recorded at spawn. Not exported.
    fn set_running(&mut self, instance_dir: PathBuf) {
        self.state = InstanceState::Running;
        self.instance_dir = Some(instance_dir);
    }

    /// Same-module mutator that records the spawned child's pid and the files
    /// of its spawn on an instance still in `Starting`, ahead of
    /// `commit_started` flipping it to `Running` via `set_running`. Lets a
    /// daemon teardown during the start window reach the child's process
    /// group, and the daemon read the key file of the child. Not exported.
    fn set_starting_process(&mut self, pid: u32, spawn_files: SpawnFiles) {
        self.pid = Some(pid);
        self.spawn_files = Some(spawn_files);
    }

    /// Same-module mutator used by `NodeEntity::mark_instance_exited` once the
    /// process has exited on its own: records the exit and how long the
    /// process ran, takes the registrations the exit ends, and moves the
    /// instance to `Restarting` with the backoff's next attempt when its policy
    /// starts it again and no removal is stopping it (`Failed` at the
    /// attempt `max_restarts` refuses), else to `Finished` for a clean exit or
    /// `Failed` for a crash. An exit the health monitor caused is a failure
    /// whatever its status, and an exit a removal asked for ends the crash
    /// loop (`attempt` 0). The pid is cleared because the process is gone, so
    /// nothing downstream tries to signal a stale (and possibly reused) pid,
    /// and the endpoints with it, which only a running process serves. The
    /// process exited at `now`.
    fn record_exit(
        &mut self,
        cause: ExitCause,
        backoff: &RestartBackoff,
        now: Instant,
        take_registrations: impl FnOnce() -> Registrations,
    ) -> ExitRecord {
        let health_kill = self.supervision.health_kill.take();
        // A process the health monitor killed ran until it last answered.
        let ran_for = self
            .supervision
            .run_started
            .take()
            .map(|since| match health_kill {
                Some(kill) => kill.last_answer.map_or(Duration::ZERO, |answer| {
                    answer.saturating_duration_since(since)
                }),
                None => now.saturating_duration_since(since),
            });
        let exit = match health_kill {
            Some(kill) => ExitCause::HealthKill {
                unanswered_secs: kill.unanswered_for.as_secs(),
            },
            None => cause,
        };
        let failed = !exit.is_clean();
        self.supervision.last_exit = Some(exit.clone());
        self.pid = None;
        self.endpoints.clear();
        let registrations = take_registrations();
        let policy = self.supervision.restart().declaration.policy();
        let policy_restarts = if failed {
            policy.restarts_after_failure()
        } else {
            policy.restarts_after_clean_exit()
        };
        let shutdown_requested = self.shutdown_requests > 0;
        let restarts_again = policy_restarts && !shutdown_requested;
        // An exit a removal asked for ends the crash loop with the instance.
        if shutdown_requested {
            self.supervision.attempt = 0;
        }
        self.state = match (restarts_again, failed) {
            (true, _) => InstanceState::Restarting,
            (false, true) => InstanceState::Failed,
            (false, false) => InstanceState::Finished,
        };
        let next = restarts_again.then(|| {
            self.supervision.registrations = registrations.clone();
            self.take_next_attempt(backoff, ran_for, now)
        });
        ExitRecord {
            exit,
            registrations,
            next,
        }
    }

    /// Takes the backoff's next attempt at `now`: the slot stays `Restarting`
    /// until it is due, or is `Failed` at the attempt `max_restarts` refuses.
    fn take_next_attempt(
        &mut self,
        backoff: &RestartBackoff,
        ran_for: Option<Duration>,
        now: Instant,
    ) -> Attempt {
        let next = self.supervision.advance_backoff(backoff, ran_for, now);
        if let Attempt::Capped { .. } = next {
            self.state = InstanceState::Failed;
        }
        next
    }

    /// Same-module mutator used by `NodeEntity::prepare_and_spawn` to start
    /// the next process of a `Restarting` instance: the slot is `Starting`
    /// again with a fresh incarnation, no pid, no endpoints, no start due, a
    /// fresh health flag the last process's monitor cannot flip, and no
    /// runtime config file (the spawn writes a new one).
    fn begin_respawn(&mut self) {
        self.state = InstanceState::Starting;
        self.incarnation = next_incarnation();
        self.pid = None;
        self.endpoints.clear();
        self.supervision.next_start = None;
        self.healthy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        if let Some(files) = self.spawn_files.take() {
            files.remove();
        }
    }

    /// Same-module mutator used by `NodeEntity::undo_start`
    /// when the next process of a `Restarting` instance fails to start: the
    /// slot waits again, with no pid.
    fn revert_respawn(&mut self) {
        self.state = InstanceState::Restarting;
        self.pid = None;
        self.spawn_files = None;
    }

    /// Same-module mutator used by `NodeEntity::commit_starting_instance` once the
    /// process is `Running`: its run starts at `now`, and a slot past its
    /// first attempt, a restart's, counts the restart.
    fn record_running(&mut self, now: Instant) {
        self.supervision.run_started = Some(now);
        self.supervision.running_since = Some(SystemTime::now());
        if self.supervision.attempt > 0 {
            self.supervision.restart_count = self.supervision.restart_count.saturating_add(1);
            // The respawn registers them in the registries once committed.
            self.supervision.registrations = Registrations::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_log::test_support::scratch_log;
    use config::node::ResolvedRestart;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    /// The files of a fixture spawn, which no test reads.
    fn fixture_spawn_files() -> SpawnFiles {
        SpawnFiles {
            runtime_config: PathBuf::from("/nonexistent/runtime_config.json5"),
            credential: PathBuf::from("/nonexistent/instance.key"),
        }
    }

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

    /// A node with three pairing slots: `arm` takes one peer, `arms` and
    /// `fleet` take any number.
    fn paired_config() -> NodeConfig {
        serde_json5::from_str::<NodeConfig>(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "sensor",
                    tag: "v1",
                    depends_on: {
                        pairings: [
                            { name: "arm_link", tag: "v1", role: "arm", link_id: "arm", cardinality: "zero_or_one" },
                            { name: "arm_link", tag: "v1", role: "arm", link_id: "arms", cardinality: "zero_or_more" },
                            { name: "joint_link", tag: "v1", role: "leader", link_id: "fleet", cardinality: "zero_or_more" },
                        ]
                    }
                },
                interfaces: {},
                execution: { language: "rust", run_cmd: ["sensor"] }
            }"#,
        )
        .expect("valid paired config")
    }

    fn built_in_launch() -> BuiltInLaunch {
        BuiltInLaunch {
            executable: PathBuf::from("/opt/peppy/bin/peppy"),
            args: vec!["mcp".to_owned(), "serve".to_owned()],
            env: vec![(
                "PEPPY_MCP_SERVE_SPEC".to_owned(),
                "/tmp/spec.json5".to_owned(),
            )],
        }
    }

    fn container_sensor_config() -> NodeConfig {
        serde_json5::from_str::<NodeConfig>(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "sensor", tag: "v1" },
                interfaces: {},
                execution: {
                    language: "python",
                    container: { def_file: "sensor.def" },
                }
            }"#,
        )
        .expect("valid container sensor config")
    }

    /// A container build whose staged tree already has an image in storage
    /// publishes that image without spawning apptainer. The def file here
    /// names nothing buildable, so an apptainer run could only fail the
    /// build.
    #[tokio::test]
    async fn a_container_build_with_a_cached_image_skips_apptainer() {
        let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
        let peppy_dirs = PeppyDirs::new(peppy_root.path());
        let working_dir = tempfile::tempdir().expect("working_dir tempdir");
        std::fs::write(working_dir.path().join("sensor.def"), b"Bootstrap: none\n")
            .expect("stage def file");

        let slot = resolve_slot(
            &peppy_dirs,
            "sensor",
            "v1",
            working_dir.path(),
            ArtifactKind::Container,
        )
        .expect("resolve slot");
        std::fs::create_dir_all(slot.path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&slot.path, b"SIF").expect("seed the cached image");

        let handle = Arc::new(RwLock::new(NodeEntity::new(
            container_sensor_config(),
            "/tmp/sensor/peppy.json5",
            Vec::new(),
        )));
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();

        let built = NodeEntity::build(
            &handle,
            BuildContext {
                working_dir: working_dir.path(),
                peppy_dirs: &peppy_dirs,
                announcer: Announcer::new(scratch_log(), feedback_tx),
                env_vars: &[],
                cancel_token: CancellationToken::new(),
                rebuild: false,
                pypi_mirror: None,
            },
        )
        .await
        .expect("a cached image makes the build succeed without apptainer");

        assert_eq!(built, slot.path);
        assert_eq!(handle.read().artifact_path(), Some(slot.path.as_path()));
        assert!(matches!(handle.read().stage(), NodeStage::Ready { .. }));
        let line = feedback_rx.try_recv().expect("the reuse is announced").line;
        assert_eq!(
            line,
            reuse_line("sensor", "v1", &slot.fingerprint, &slot.path)
        );
    }

    #[test]
    fn a_built_in_entity_is_ready_with_the_executable_as_its_artifact() {
        let entity = NodeEntity::built_in(
            sensor_config(),
            PathBuf::from("/tmp/built_in/sensor/peppy.json5"),
            built_in_launch(),
            Vec::new(),
        );
        assert_eq!(entity.stage().to_serialized(), SerializedNodeStage::Ready);
        assert!(
            matches!(
                entity.stage(),
                NodeStage::Ready {
                    artifact: Artifact::BuiltIn(_),
                    ..
                }
            ),
            "the recipe is the entity's artifact"
        );
        assert_eq!(
            entity.artifact_path(),
            Some(Path::new("/opt/peppy/bin/peppy"))
        );
        assert_eq!(
            entity.config_path(),
            Path::new("/tmp/built_in/sensor/peppy.json5")
        );
        assert_eq!(entity.built_in_launch(), Some(&built_in_launch()));
        assert!(entity.instances().is_empty());
        assert!(
            entity.stage().ensure_spawnable().is_ok(),
            "a built-in node spawns without a build"
        );
        assert!(
            NodeEntity::new(sensor_config(), "/tmp/sensor/peppy.json5", Vec::new())
                .built_in_launch()
                .is_none(),
            "a sourced node carries no recipe"
        );
    }

    /// A commit the entity no longer takes, its generation replaced since the
    /// spawn, kills the whole process group of the start, a forked child
    /// that ignores SIGTERM included.
    #[tokio::test]
    async fn a_commit_on_a_stale_generation_kills_the_process_group() {
        let handle = Arc::new(RwLock::new(NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/opt/peppy/bin/peppy")),
            Vec::new(),
        )));
        let instance_id = Name::new("stale_inst").unwrap();
        let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
        let instance_dir = PeppyDirs::new(peppy_root.path())
            .instances_dir()
            .join("stale_inst");
        std::fs::create_dir_all(&instance_dir).expect("instance dir");
        let child = tokio::process::Command::new("sh")
            .args(["-c", crate::FORKS_A_TERM_IGNORER])
            .process_group(0)
            .spawn()
            .expect("spawn a forking node");
        let pid = child.id().expect("the child has a pid");
        /// Kills the test's group when the test ends, a failed one included.
        struct KillGroupOnDrop(u32);
        impl Drop for KillGroupOnDrop {
            fn drop(&mut self) {
                crate::kill_process_group(self.0);
            }
        }
        let _group = KillGroupOnDrop(pid);
        let forked = tokio::time::timeout(Duration::from_secs(5), async {
            while crate::process_group_size(pid) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(forked.is_ok(), "the node forked its child");
        let stale_generation = handle.read().generation + 1;
        let started_ctx = register_start(
            &handle,
            &instance_id,
            &child,
            &instance_dir,
            stale_generation,
        );

        let committed = NodeEntity::commit_started(
            &handle,
            child,
            started_ctx,
            instance_id,
            Vec::new(),
            Instant::now(),
        )
        .await;
        assert!(
            matches!(committed, Err(Error::InvalidStageTransition { .. })),
            "a stale generation refuses the commit"
        );
        let emptied = tokio::time::timeout(Duration::from_secs(5), async {
            while crate::process_group_exists(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(emptied.is_ok(), "the group led by {pid} is empty");
    }

    /// The endpoints the start sequence records at commit reach the
    /// serialized instance with their kind and every URL, and leave with the
    /// process at its exit.
    #[tokio::test]
    async fn serialized_instances_carry_the_endpoints_committed_at_start() {
        let instance_id = Name::new("panel_inst").unwrap();
        let handle = ready_entity_with_all(Vec::new());
        let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
        let peppy_dirs = PeppyDirs::new(peppy_root.path());
        let instance_dir = peppy_dirs.instances_dir().join("panel_inst");
        std::fs::create_dir_all(&instance_dir).expect("instance dir");
        let child = tokio::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let generation = handle.read().generation;
        let started_ctx = register_start(&handle, &instance_id, &child, &instance_dir, generation);
        let endpoints = vec![InstanceEndpoint {
            label: "panel".to_owned(),
            kind: config::node::EndpointKind::Page,
            urls: vec![
                "http://127.0.0.1:8765".to_owned(),
                "http://192.168.1.5:8765".to_owned(),
            ],
        }];
        let mut child = NodeEntity::commit_started(
            &handle,
            child,
            started_ctx,
            instance_id.clone(),
            endpoints.clone(),
            Instant::now(),
        )
        .await
        .expect("commit_started should succeed");
        let _ = child.start_kill();
        let _ = child.wait().await;

        let serialized = serialize_node_entity(&handle.read(), "core_a");
        assert_eq!(serialized.instances[0].endpoints, endpoints);
        assert_eq!(
            handle.read().instances()[0].endpoints(),
            endpoints.as_slice()
        );
        let supervision = handle.read().instances()[0].supervision().clone();
        assert!(
            supervision.run_started.is_some() && supervision.running_since.is_some(),
            "the commit starts the clock of the run"
        );
        assert_eq!(
            supervision.first_start.map(|first| first.goal),
            Some(FirstStart::fixture(&instance_id, "sensor", "v1").goal)
        );
        assert_eq!(serialized.instances[0].restart_status.restart_count, 0);
        assert!(
            serialized.instances[0]
                .restart_status
                .running_since_ms
                .is_some()
        );

        let pid = handle.read().instances()[0]
            .pid()
            .expect("a running instance has a pid");
        NodeEntity::mark_instance_exited(
            &handle,
            &instance_id,
            pid,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("the running instance exits");
        assert!(
            handle.read().instances()[0].endpoints().is_empty(),
            "only a running process serves its endpoints"
        );
    }

    fn on_failure() -> ResolvedRestart {
        ResolvedRestart {
            declaration: config::node::RestartDeclaration::policy_only(
                config::node::RestartPolicy::OnFailure,
            ),
            origin: config::node::RestartOrigin::Node,
        }
    }

    fn always() -> ResolvedRestart {
        ResolvedRestart {
            declaration: config::node::RestartDeclaration::policy_only(
                config::node::RestartPolicy::Always,
            ),
            origin: config::node::RestartOrigin::Node,
        }
    }

    /// `on_failure` with a cap of `max_restarts` attempts in a row.
    fn capped(max_restarts: u32) -> ResolvedRestart {
        ResolvedRestart {
            declaration: config::node::RestartDeclaration::new(
                config::node::RestartPolicy::OnFailure,
                std::num::NonZeroU32::new(max_restarts),
            )
            .expect("a cap on a restarting policy"),
            origin: config::node::RestartOrigin::Node,
        }
    }

    /// A backoff of 2 s doubling to a minute, reset after two minutes.
    const BACKOFF: RestartBackoff = RestartBackoff {
        first_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(60),
        reset_after: Duration::from_secs(120),
    };

    /// The pid of the process a test's running slot runs.
    const PID: u32 = 7;

    /// A `Restarting` slot as `mark_instance_exited` leaves it, with the goal
    /// a respawn starts from.
    fn waiting_instance(id: &Name) -> TrackedNodeInstance {
        waiting_instance_under(id, on_failure())
    }

    /// [`waiting_instance`] under `restart`.
    fn waiting_instance_under(id: &Name, restart: ResolvedRestart) -> TrackedNodeInstance {
        let mut instance = running_under(id, restart, 0);
        instance.record_exit(
            ExitCause::signal(9, false),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        );
        instance
    }

    /// A `Running` instance first started under `restart`, `attempt`
    /// attempts into its backoff.
    fn running_under(id: &Name, restart: ResolvedRestart, attempt: u32) -> TrackedNodeInstance {
        let mut instance =
            TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new())
                .with_first_start(FirstStart {
                    restart,
                    ..FirstStart::fixture(id, "sensor", "v1")
                });
        instance.set_starting_process(PID, fixture_spawn_files());
        instance.record_running(Instant::now());
        instance.supervision.attempt = attempt;
        instance
    }

    /// A stop claims an instance only while it runs the process the stop
    /// asked to exit: another process, none, or one already claimed stays
    /// as it is.
    #[test]
    fn claim_process_takes_only_the_process_asked_to_exit() {
        let id = Name::new("removed-1").unwrap();
        let mut running =
            TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new());
        running.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(running);
        assert_eq!(
            handle.read().claim_process(&id, PID + 1),
            ProcessClaim::Gone
        );
        assert!(!handle.read().instances()[0].is_stopping());
        assert_eq!(handle.read().claim_process(&id, PID), ProcessClaim::Claimed);
        assert!(handle.read().instances()[0].is_stopping());
        assert_eq!(
            handle.read().claim_process(&id, PID),
            ProcessClaim::ClaimedElsewhere,
            "a claimed process is claimed once"
        );

        let waiting = Name::new("removed-2").unwrap();
        let handle = ready_entity_with(waiting_instance(&waiting));
        let token = handle.read().instances()[0]
            .supervision()
            .restart_token
            .clone();
        assert_eq!(
            handle.read().claim_process(&waiting, PID),
            ProcessClaim::Gone
        );
        assert!(!handle.read().instances()[0].is_stopping());
        assert!(!token.is_cancelled());
    }

    /// A stop claims the start of a restart, which cancels it and hands
    /// back what the slot kept; a first start and a start a stop already
    /// claimed are left alone.
    #[test]
    fn claim_starting_takes_only_an_unclaimed_restarts_start() {
        let restart = Name::new("restarting-9").unwrap();
        let mut respawning = waiting_instance(&restart);
        respawning.begin_respawn();
        let token = respawning.supervision.restart_token.clone();
        let first = Name::new("first-9").unwrap();
        let first_start =
            TrackedNodeInstance::new(first.clone(), InstanceState::Starting, BTreeMap::new());
        let handle = ready_entity_with_all(vec![respawning, first_start]);

        assert_eq!(
            handle.read().claim_starting(&first),
            StartingClaim::FirstStart
        );
        assert_eq!(
            handle.read().claim_starting(&restart),
            StartingClaim::Claimed(KeptRelations::default())
        );
        assert!(token.is_cancelled(), "the claim cancels the start");
        assert!(handle.read().instances()[0].is_stopping());
        assert_eq!(
            handle.read().claim_starting(&restart),
            StartingClaim::ClaimedElsewhere,
            "a claimed start is not claimed twice"
        );
        assert_eq!(
            handle
                .read()
                .claim_starting(&Name::new("absent-9").unwrap()),
            StartingClaim::NotStarting
        );
    }

    /// An exit while a removal's shutdown request is noted ends the instance
    /// whatever its policy and its crash loop with it, a failed exit
    /// included; once every removal withdrew its note the policy applies.
    #[test]
    fn an_exit_a_removal_asked_for_ends_the_instance() {
        let crashed = Name::new("removed-0").unwrap();
        let handle = ready_entity_with(running_under(&crashed, on_failure(), 3));
        handle.write().note_shutdown_request(&crashed, PID);
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &crashed,
            PID,
            ExitCause::signal(15, false),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert!(exited.next.is_none(), "{:?}", exited.next);
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Failed);
        assert_eq!(handle.read().instances()[0].supervision().attempt, 0);

        let id = Name::new("removed-1").unwrap();
        let handle = ready_entity_with(running_under(&id, always(), 0));
        handle.write().note_shutdown_request(&id, PID);
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &id,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert!(exited.next.is_none(), "{:?}", exited.next);
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );

        let withdrawn = Name::new("withdrawn-1").unwrap();
        let handle = ready_entity_with(running_under(&withdrawn, always(), 0));
        handle.write().note_shutdown_request(&withdrawn, PID);
        handle.write().withdraw_shutdown_request(&withdrawn, PID);
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &withdrawn,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert!(exited.next.is_some());
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );

        // Two removals note at once: one withdrawing its note leaves the
        // other's standing, and the instance restarts once both withdrew.
        let exit_after_notes = |withdrawals: usize| {
            let id = Name::new("twice-noted").unwrap();
            let handle = ready_entity_with(running_under(&id, always(), 0));
            handle.write().note_shutdown_request(&id, PID);
            handle.write().note_shutdown_request(&id, PID);
            for _ in 0..withdrawals {
                handle.write().withdraw_shutdown_request(&id, PID);
            }
            NodeEntity::mark_instance_exited(
                &handle,
                &id,
                PID,
                ExitCause::Code(0),
                &BACKOFF,
                Instant::now(),
                Registrations::default,
            )
            .expect("a running instance exits");
            handle.read().instances()[0].state()
        };
        assert_eq!(exit_after_notes(1), InstanceState::Finished);
        assert_eq!(exit_after_notes(2), InstanceState::Restarting);

        // A withdrawal for another process under the id, the instance a
        // removal noted having left meanwhile, leaves the note standing.
        let id = Name::new("noted-again").unwrap();
        let handle = ready_entity_with(running_under(&id, always(), 0));
        handle.write().note_shutdown_request(&id, PID);
        handle.write().withdraw_shutdown_request(&id, PID + 1);
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &id,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert!(exited.next.is_none(), "{:?}", exited.next);
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );
    }

    /// An abandoned start's group, when it forked one, is returned for the
    /// kill, then its slot is undone: a respawn's slot waits again, a claimed
    /// one leaves, and a first start's leaves; another run's start under the
    /// id is left to that run.
    #[test]
    fn an_abandoned_start_is_undone() {
        let group = |is_container| {
            Some(ProcessGroup {
                pid: PID,
                is_container,
            })
        };
        let respawning = |id: &Name| {
            let mut instance = waiting_instance(id);
            instance.begin_respawn();
            instance
        };
        // The run of the respawn of the slot's instance, begun at `restart_count`.
        let respawn_run =
            |handle: &Arc<RwLock<NodeEntity>>, restart_count: u32| StartRun::Respawn {
                goal: handle.read().instances()[0]
                    .supervision()
                    .first_start
                    .as_ref()
                    .map(|first| Arc::clone(&first.goal))
                    .expect("a restarting slot keeps its first start"),
                restart_count,
            };

        let id = Name::new("abandoned-1").unwrap();
        let mut forked = respawning(&id);
        forked.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(forked);
        let own_goal = handle.read().instances()[0]
            .supervision()
            .first_start
            .as_ref()
            .map(|first| Arc::clone(&first.goal))
            .expect("a restarting slot keeps its first start");
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &id, &StartRun::New(Arc::clone(&own_goal))),
            None,
            "a restart's start is no first start's, even of its own goal"
        );
        assert!(!NodeEntity::undo_abandoned_start(
            &handle,
            &id,
            &StartRun::New(Arc::clone(&own_goal))
        ));
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Starting
        );
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &id, &respawn_run(&handle, 0)),
            group(false)
        );
        assert!(NodeEntity::undo_abandoned_start(
            &handle,
            &id,
            &respawn_run(&handle, 0)
        ));
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );
        assert_eq!(handle.read().instances()[0].pid(), None);

        let claimed = Name::new("abandoned-2").unwrap();
        let mut forked = respawning(&claimed);
        forked.set_starting_process(PID, fixture_spawn_files());
        forked.mark_stopping();
        let handle = ready_entity_with(forked);
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &claimed, &respawn_run(&handle, 0)),
            group(false)
        );
        NodeEntity::undo_abandoned_start(&handle, &claimed, &respawn_run(&handle, 0));
        assert!(
            handle.read().instances().is_empty(),
            "a claimed start leaves with its slot"
        );

        let unforked = Name::new("abandoned-3").unwrap();
        let handle = ready_entity_with(respawning(&unforked));
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &unforked, &respawn_run(&handle, 0)),
            None,
            "a start that never forked leaves no group"
        );
        NodeEntity::undo_abandoned_start(&handle, &unforked, &respawn_run(&handle, 0));
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );

        let later = Name::new("abandoned-later").unwrap();
        let mut forked = respawning(&later);
        forked.supervision.restart_count = 1;
        forked.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(forked);
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &later, &respawn_run(&handle, 0)),
            None,
            "a later restart's start is not an earlier respawn's"
        );
        assert!(!NodeEntity::undo_abandoned_start(
            &handle,
            &later,
            &respawn_run(&handle, 0)
        ));
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Starting,
            "a later restart's start is left to it"
        );

        let not_begun = Name::new("abandoned-not-begun").unwrap();
        let handle = ready_entity_with(waiting_instance(&not_begun));
        assert!(
            NodeEntity::undo_abandoned_start(&handle, &not_begun, &respawn_run(&handle, 0)),
            "a respawn that had not begun its start leaves its restart open"
        );
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );

        let later_waiting = Name::new("abandoned-later-waiting").unwrap();
        let mut waiting = waiting_instance(&later_waiting);
        waiting.supervision.restart_count = 1;
        let handle = ready_entity_with(waiting);
        assert!(
            !NodeEntity::undo_abandoned_start(&handle, &later_waiting, &respawn_run(&handle, 0)),
            "a later restart awaiting its start is not an earlier respawn's"
        );
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );

        let replaced = Name::new("abandoned-replaced").unwrap();
        let mut forked = respawning(&replaced);
        forked.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(forked);
        let earlier_instance = StartRun::Respawn {
            goal: Arc::clone(&FirstStart::fixture(&replaced, "sensor", "v1").goal),
            restart_count: 0,
        };
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &replaced, &earlier_instance),
            None,
            "a restart's start of a later instance under the id is not an earlier one's"
        );
        assert!(!NodeEntity::undo_abandoned_start(
            &handle,
            &replaced,
            &earlier_instance
        ));
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Starting
        );

        let container = Name::new("abandoned-4").unwrap();
        let mut forked = respawning(&container);
        forked.set_starting_process(PID, fixture_spawn_files());
        let handle = entity_of(container_sensor_config(), vec![forked]);
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &container, &respawn_run(&handle, 0)),
            group(true),
            "a container's group carries its in-VM kill"
        );

        let first = Name::new("first-5").unwrap();
        let first_start = FirstStart::fixture(&first, "sensor", "v1");
        let goal = Arc::clone(&first_start.goal);
        let mut starting =
            TrackedNodeInstance::new(first.clone(), InstanceState::Starting, BTreeMap::new())
                .with_first_start(first_start);
        starting.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(starting);
        let another_goal = Arc::clone(&FirstStart::fixture(&first, "sensor", "v1").goal);
        for other_run in [
            StartRun::New(another_goal),
            StartRun::Respawn {
                goal: Arc::clone(&goal),
                restart_count: 0,
            },
        ] {
            assert_eq!(
                NodeEntity::abandoned_start_group(&handle, &first, &other_run),
                None
            );
            NodeEntity::undo_abandoned_start(&handle, &first, &other_run);
            assert_eq!(
                handle.read().instances()[0].state(),
                InstanceState::Starting,
                "another run's start is left to it"
            );
        }
        assert_eq!(
            NodeEntity::abandoned_start_group(&handle, &first, &StartRun::New(Arc::clone(&goal))),
            group(false)
        );
        NodeEntity::undo_abandoned_start(&handle, &first, &StartRun::New(Arc::clone(&goal)));
        assert!(
            handle.read().instances().is_empty(),
            "a first start leaves with its slot"
        );
    }

    /// A respawn's commit counts the restart and drops the registrations the
    /// slot kept for it, which the respawn has put in the registries; until
    /// then its start keeps them for a stop to announce to.
    #[test]
    fn a_respawns_commit_clears_the_kept_registrations() {
        let id = Name::new("respawned-1").unwrap();
        let kept = Registrations {
            observations: BTreeMap::new(),
            watchers: vec!["cn-atlas".to_owned()],
        };
        let mut instance = running_under(&id, on_failure(), 0);
        instance.record_exit(
            ExitCause::signal(9, false),
            &BACKOFF,
            Instant::now(),
            || kept.clone(),
        );
        instance.begin_respawn();
        assert_eq!(instance.supervision.registrations, kept);

        instance.record_running(Instant::now());
        assert_eq!(instance.supervision.registrations, Registrations::default());
        assert_eq!(instance.supervision.restart_count, 1);
    }

    /// The health monitor's kill of a process that never answered a health
    /// check, `unanswered_secs` into its silence.
    fn never_answered(unanswered_secs: u64) -> HealthKill {
        HealthKill {
            unanswered_for: Duration::from_secs(unanswered_secs),
            last_answer: None,
        }
    }

    /// A run the health monitor ends counts up to the last probe its process
    /// answered, as a self-exit's run counts up to its exit: one silent past
    /// the reset window still backs off further.
    #[test]
    fn a_health_killed_run_counts_up_to_its_last_answer() {
        let reset_after = BACKOFF.reset_after;
        for (last_answer_after, expected_attempt) in [
            (None, 4),
            (Some(reset_after / 2), 4),
            (Some(reset_after), 1),
        ] {
            let id = Name::new("hung-7").unwrap();
            let mut instance = running_under(&id, on_failure(), 3);
            let started = Instant::now()
                .checked_sub(reset_after * 2)
                .expect("the clock is past twice the reset window");
            instance.supervision.run_started = Some(started);
            let handle = ready_entity_with(instance);
            let kill = HealthKill {
                unanswered_for: Duration::from_secs(60),
                last_answer: last_answer_after.map(|after| started + after),
            };
            assert!(handle.write().arm_health_kill(id.as_str(), kill).is_some());
            NodeEntity::mark_instance_exited(
                &handle,
                &id,
                PID,
                ExitCause::Code(0),
                &BACKOFF,
                Instant::now(),
                Registrations::default,
            )
            .expect("a running instance exits");
            assert_eq!(
                handle.read().instances()[0].supervision().attempt,
                expected_attempt,
                "last answer {last_answer_after:?} after the start"
            );
        }
    }

    /// Registers `child` as the start of `instance_id` on the `Ready` entity
    /// `handle`, a `Starting` slot with a first start and the child's pid, and
    /// hands back the context `prepare_and_spawn` would for a start under
    /// `generation`, with its files under `instance_dir`.
    fn register_start(
        handle: &Arc<RwLock<NodeEntity>>,
        instance_id: &Name,
        child: &Child,
        instance_dir: &Path,
        generation: u64,
    ) -> StartedInstanceCtx {
        let pid = child.id().expect("the child has a pid");
        let mut guard = handle.write();
        let NodeStage::Ready { instances, .. } = &mut guard.stage else {
            panic!("from_snapshot builds a Ready entity")
        };
        let mut starting = TrackedNodeInstance::new(
            instance_id.clone(),
            InstanceState::Starting,
            BTreeMap::new(),
        )
        .with_first_start(FirstStart::fixture(instance_id, "sensor", "v1"));
        starting.set_starting_process(pid, fixture_spawn_files());
        instances.push(starting);
        StartedInstanceCtx {
            instance_dir: instance_dir.to_path_buf(),
            spawn_files: SpawnFiles {
                runtime_config: instance_dir.join("peppy_runtime.json5"),
                credential: instance_dir.join("instance.key"),
            },
            stderr_buffer: StderrTail::new(),
            output_reader_handles: Vec::new(),
            group: ProcessGroup {
                pid,
                is_container: false,
            },
            generation,
        }
    }

    /// A `sleep` child and the context `prepare_and_spawn` hands back for
    /// its start as `instance_id` on an entity of generation `generation`.
    /// The instance dir and the runtime config file of the start exist under
    /// `peppy_root`, so a test can see whether the commit removes them.
    fn sleeping_start(
        peppy_root: &Path,
        instance_id: &str,
        generation: u64,
    ) -> (Child, StartedInstanceCtx) {
        let instance_dir = PeppyDirs::new(peppy_root).instances_dir().join(instance_id);
        std::fs::create_dir_all(&instance_dir).expect("instance dir");
        let runtime_config_path = peppy_root.join(format!("runtime_config_{instance_id}.json5"));
        std::fs::write(&runtime_config_path, "{}").expect("runtime config file");
        let credential = peppy_root.join(format!("{instance_id}.key"));
        std::fs::write(&credential, "{}").expect("key file");
        let child = tokio::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let group = ProcessGroup {
            pid: child.id().expect("the child has a pid"),
            is_container: false,
        };
        (
            child,
            StartedInstanceCtx {
                instance_dir,
                spawn_files: SpawnFiles {
                    runtime_config: runtime_config_path,
                    credential,
                },
                stderr_buffer: StderrTail::new(),
                output_reader_handles: Vec::new(),
                group,
                generation,
            },
        )
    }

    /// Commits the start of `sensor_inst` on an entity of generation
    /// `generation` and checks that `commit_started` refuses it as the
    /// transition `from` to `to`, and that the refusal removes the instance
    /// dir and the runtime config file of the start.
    async fn assert_commit_refused(
        handle: &Arc<RwLock<NodeEntity>>,
        generation: u64,
        from: &str,
        to: &str,
    ) {
        let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
        let (child, started_ctx) = sleeping_start(peppy_root.path(), "sensor_inst", generation);
        let instance_dir = started_ctx.instance_dir.clone();
        let spawn_files = started_ctx.spawn_files.clone();

        let error = NodeEntity::commit_started(
            handle,
            child,
            started_ctx,
            Name::new("sensor_inst").unwrap(),
            Vec::new(),
            Instant::now(),
        )
        .await
        .expect_err("commit_started should refuse the start");

        let Error::InvalidStageTransition {
            node_name,
            node_tag,
            from: refused_from,
            to: refused_to,
        } = error
        else {
            panic!("expected InvalidStageTransition, got {error:?}");
        };
        assert_eq!(
            (
                node_name.as_str(),
                node_tag.as_str(),
                refused_from,
                refused_to
            ),
            ("sensor", "v1", from, to)
        );
        assert!(
            !instance_dir.exists(),
            "a refused commit removes the instance dir"
        );
        assert!(
            !spawn_files.runtime_config.exists(),
            "a refused commit removes the runtime config file"
        );
        assert!(
            !spawn_files.credential.exists(),
            "a refused commit removes the key file"
        );
    }

    /// A concurrent `push_config` gives the entity a new generation: the
    /// commit leaves the replacement entity's instance alone.
    #[tokio::test]
    async fn commit_started_refuses_a_replaced_entity() {
        let handle = ready_entity_with(TrackedNodeInstance::new(
            Name::new("sensor_inst").unwrap(),
            InstanceState::Starting,
            BTreeMap::new(),
        ));
        let replaced_generation = handle.read().generation + 1;

        assert_commit_refused(&handle, replaced_generation, "stale-generation", "Running").await;

        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Starting
        );
    }

    #[tokio::test]
    async fn commit_started_refuses_an_instance_the_entity_does_not_track() {
        let handle = ready_entity_with(TrackedNodeInstance::new(
            Name::new("other_inst").unwrap(),
            InstanceState::Starting,
            BTreeMap::new(),
        ));
        let generation = handle.read().generation;

        assert_commit_refused(&handle, generation, "missing", "Running").await;
    }

    #[tokio::test]
    async fn commit_started_refuses_an_instance_that_is_not_starting() {
        let handle = ready_entity_with(TrackedNodeInstance::new(
            Name::new("sensor_inst").unwrap(),
            InstanceState::Running,
            BTreeMap::new(),
        ));
        let generation = handle.read().generation;

        assert_commit_refused(&handle, generation, "Running", "Running").await;

        let guard = handle.read();
        assert_eq!(guard.instances()[0].state(), InstanceState::Running);
        assert_eq!(
            guard.instances()[0].instance_dir(),
            None,
            "the refused commit records no instance dir"
        );
    }

    #[tokio::test]
    async fn commit_started_refuses_an_entity_that_is_not_ready() {
        let handle = Arc::new(RwLock::new(NodeEntity::new(
            sensor_config(),
            "/tmp/sensor/peppy.json5",
            Vec::new(),
        )));
        let generation = handle.read().generation;

        assert_commit_refused(&handle, generation, "Added", "Ready").await;
    }

    /// A delivered producer set replaces the one instance's slot it names and
    /// nothing else; an instance the entity does not track is reported.
    #[test]
    fn a_delivered_set_replaces_the_named_slot_of_the_named_instance() {
        let planner = TrackedNodeInstance::new(
            Name::new("planner_inst").unwrap(),
            InstanceState::Running,
            BTreeMap::from([(
                "main".to_string(),
                config::runtime::BoundProducers::from(config::runtime::ProducerRef::new(
                    "core_a", "camera_1",
                )),
            )]),
        );
        let mut entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/opt/peppy/bin/peppy")),
            vec![planner],
        );
        let grown = config::runtime::BoundProducers::try_from(vec![
            config::runtime::ProducerRef::new("core_a", "alpha_arm_inst"),
            config::runtime::ProducerRef::new("core_b", "bravo_arm_inst"),
        ])
        .unwrap();

        assert!(entity.set_instance_slot_binding(
            &Name::new("planner_inst").unwrap(),
            "robots",
            grown.clone(),
        ));
        let bindings = entity.instances()[0].slot_bindings();
        assert_eq!(bindings["robots"], grown);
        assert_eq!(
            bindings["main"].as_slice()[0].producer.instance_id,
            "camera_1"
        );
        assert!(!entity.set_instance_slot_binding(
            &Name::new("absent_inst").unwrap(),
            "robots",
            grown,
        ));
    }

    /// Guards the one line that places resolved bindings onto the `graph_json`
    /// wire: `serialize_node_entity` must copy each instance's `slot_bindings`
    /// through to its `SerializedInstance`. Setting that field to an empty map
    /// makes this fail.
    #[test]
    fn serialized_node_carries_per_instance_slot_bindings() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "arm".to_string(),
            config::runtime::BoundProducers::from(config::runtime::ProducerRef::new(
                "core_a", "arm-1",
            )),
        );
        // A multi-cardinality slot's ordered set rides the same wire.
        bindings.insert(
            "cameras".to_string(),
            config::runtime::BoundProducers::try_from(vec![
                config::runtime::ProducerRef::new("core_a", "cam-1"),
                config::runtime::ProducerRef::new("core_a", "cam-2"),
            ])
            .expect("distinct producers"),
        );
        let bound = TrackedNodeInstance::new(
            Name::new("sensor-1").unwrap(),
            InstanceState::Running,
            bindings.clone(),
        );
        // A second, bindless instance must round-trip as an empty map.
        let unbound = TrackedNodeInstance::new(
            Name::new("sensor-2").unwrap(),
            InstanceState::Running,
            BTreeMap::new(),
        );
        let entity = NodeEntity::from_snapshot(
            sensor_config(),
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            vec![bound, unbound],
        );

        let serialized = serialize_node_entity(&entity, "core_a");
        assert_eq!(serialized.core_node, "core_a");
        assert_eq!(serialized.instances.len(), 2);
        assert_eq!(serialized.instances[0].instance_id, "sensor-1");
        assert_eq!(serialized.instances[0].slot_bindings, bindings);
        assert!(serialized.instances[1].slot_bindings.is_empty());
    }

    /// Guards the one line that places per-instance health onto the
    /// `graph_json` wire: `serialize_node_entity` must copy each instance's
    /// `healthy()` through to its `SerializedInstance`. Hardcoding that line to
    /// `true` (or dropping it) makes the unhealthy assertion fail. Also the
    /// only coverage of the `healthy()`/`set_healthy()` pair: a fresh instance
    /// defaults to healthy, and `set_healthy(false)` flips it.
    #[test]
    fn serialized_node_carries_per_instance_health() {
        let healthy = TrackedNodeInstance::new(
            Name::new("sensor-1").unwrap(),
            InstanceState::Running,
            BTreeMap::new(),
        );
        assert!(
            healthy.healthy(),
            "a freshly-created instance should default to healthy"
        );
        let unhealthy = TrackedNodeInstance::new(
            Name::new("sensor-2").unwrap(),
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

        let serialized = serialize_node_entity(&entity, "core_a");
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

    /// Builds a `Ready` entity behind a handle that tracks the one
    /// `instance`, the shape `mark_instance_exited` and `commit_started`
    /// operate on.
    fn ready_entity_with(instance: TrackedNodeInstance) -> Arc<RwLock<NodeEntity>> {
        ready_entity_with_all(vec![instance])
    }

    /// [`ready_entity_with`] for a node that declares [`paired_config`]'s
    /// pairing slots.
    fn paired_entity_with(instance: TrackedNodeInstance) -> Arc<RwLock<NodeEntity>> {
        entity_of(paired_config(), vec![instance])
    }

    fn ready_entity_with_all(instances: Vec<TrackedNodeInstance>) -> Arc<RwLock<NodeEntity>> {
        entity_of(sensor_config(), instances)
    }

    /// A `Ready` entity of `config` holding `instances`, behind a handle.
    fn entity_of(
        config: NodeConfig,
        instances: Vec<TrackedNodeInstance>,
    ) -> Arc<RwLock<NodeEntity>> {
        Arc::new(RwLock::new(NodeEntity::from_snapshot(
            config,
            PathBuf::from("/tmp/sensor/peppy.json5"),
            Some(PathBuf::from("/tmp/sensor.sif")),
            instances,
        )))
    }

    #[test]
    fn mark_instance_exited_moves_running_to_finished_on_clean_exit() {
        let id = Name::new("one-shot-1").unwrap();
        let mut instance =
            TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new());
        instance.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(instance);

        let taken = Registrations {
            observations: BTreeMap::new(),
            watchers: vec!["cn-atlas".to_owned()],
        };
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &id,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            || taken.clone(),
        )
        .expect("a running instance exits");
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );
        assert_eq!(exited.exit, ExitCause::Code(0));
        assert_eq!(
            exited.registrations, taken,
            "the exit takes the registrations whatever the policy"
        );
        assert!(exited.next.is_none(), "`never` schedules nothing");
        {
            let guard = handle.read();
            let inst = &guard.instances()[0];
            assert_eq!(inst.state(), InstanceState::Finished);
            assert_eq!(inst.pid(), None, "pid is cleared once the process is gone");
            assert_eq!(inst.supervision().last_exit, Some(ExitCause::Code(0)));
        }

        // A second exit is a no-op: the instance is already terminal.
        assert!(
            NodeEntity::mark_instance_exited(
                &handle,
                &id,
                PID,
                ExitCause::Code(1),
                &BACKOFF,
                Instant::now(),
                || { panic!("a terminal instance takes no registrations") }
            )
            .is_none()
        );
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );
    }

    #[test]
    fn mark_instance_exited_moves_running_to_failed_on_unclean_exit() {
        let id = Name::new("crash-1").unwrap();
        let mut instance =
            TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new());
        instance.set_starting_process(PID, fixture_spawn_files());
        let handle = ready_entity_with(instance);

        let taken = Registrations {
            observations: BTreeMap::new(),
            watchers: vec!["cn-atlas".to_owned()],
        };
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &id,
            PID,
            ExitCause::signal(9, false),
            &BACKOFF,
            Instant::now(),
            || taken.clone(),
        )
        .expect("a running instance exits");
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Failed);
        assert_eq!(exited.exit, ExitCause::signal(9, false));
        assert_eq!(exited.registrations, taken);
        assert!(exited.next.is_none());
    }

    /// Under a policy that restarts, a crash leaves the slot `Restarting`
    /// with the exit, the registrations and the backoff's next attempt
    /// recorded, the run's length deciding whether the backoff starts over;
    /// a clean exit under `on_failure` is `Finished`; a health kill is a
    /// failure whatever the status; the attempt `max_restarts` refuses
    /// leaves the slot `Failed`.
    #[test]
    fn mark_instance_exited_takes_the_next_attempt_under_the_policy() {
        let id = Name::new("crash-2").unwrap();
        let handle = ready_entity_with(running_under(&id, on_failure(), 5));
        let taken = Registrations {
            observations: BTreeMap::new(),
            watchers: vec!["cn-atlas".to_owned()],
        };
        let reset_after_a_short_run = RestartBackoff {
            reset_after: Duration::from_millis(20),
            ..BACKOFF
        };
        std::thread::sleep(Duration::from_millis(20));
        let before = SystemTime::now();
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &id,
            PID,
            ExitCause::signal(9, false),
            &reset_after_a_short_run,
            Instant::now(),
            || taken.clone(),
        )
        .expect("a running instance exits");
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );
        assert_eq!(exited.exit, ExitCause::signal(9, false));
        assert_eq!(exited.registrations, taken);
        let Some(Attempt::Scheduled { attempt, delay, .. }) = exited.next else {
            panic!("a restarting policy schedules: {:?}", exited.next)
        };
        assert_eq!(
            attempt.get(),
            1,
            "the run was measured: it passed the reset window"
        );
        assert_eq!(delay, BACKOFF.first_delay);
        {
            let guard = handle.read();
            let supervision = guard.instances()[0].supervision();
            assert_eq!(supervision.last_exit, Some(ExitCause::signal(9, false)));
            assert_eq!(supervision.attempt, 1);
            assert!(
                supervision.next_start.expect("a start is due") >= before + delay,
                "the next start is the delay away"
            );
            assert_eq!(
                supervision.registrations, taken,
                "a restarting slot keeps what the exit took"
            );
            assert_eq!(
                supervision.run_started, None,
                "the clock of the run stops at exit"
            );
            assert!(
                supervision.running_since.is_some(),
                "when the instance last ran is kept"
            );
        }

        let climbing = Name::new("crash-3").unwrap();
        let handle = ready_entity_with(running_under(&climbing, on_failure(), 5));
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &climbing,
            PID,
            ExitCause::Code(1),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert!(
            matches!(
                exited.next,
                Some(Attempt::Scheduled { attempt, delay, .. })
                    if attempt.get() == 6 && delay == BACKOFF.max_delay
            ),
            "a short run backs off further: {:?}",
            exited.next
        );

        let clean = Name::new("done-2").unwrap();
        let handle = ready_entity_with(running_under(&clean, on_failure(), 0));
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &clean,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Finished
        );
        assert!(exited.next.is_none());

        let killed = Name::new("hung-2").unwrap();
        let handle = ready_entity_with(running_under(&killed, on_failure(), 0));
        assert_eq!(
            handle
                .write()
                .arm_health_kill(killed.as_str(), never_answered(60)),
            Some(ProcessGroup {
                pid: PID,
                is_container: false
            })
        );
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &killed,
            PID,
            ExitCause::Code(0),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Restarting
        );
        assert_eq!(
            exited.exit,
            ExitCause::HealthKill {
                unanswered_secs: 60
            }
        );
        assert_eq!(
            handle.read().instances()[0].supervision().last_exit,
            Some(ExitCause::HealthKill {
                unanswered_secs: 60
            })
        );

        let capped_id = Name::new("capped-2").unwrap();
        let handle = ready_entity_with(running_under(&capped_id, capped(2), 2));
        let exited = NodeEntity::mark_instance_exited(
            &handle,
            &capped_id,
            PID,
            ExitCause::signal(9, false),
            &BACKOFF,
            Instant::now(),
            Registrations::default,
        )
        .expect("a running instance exits");
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Failed);
        assert!(
            matches!(
                exited.next,
                Some(Attempt::Capped { max_restarts, .. }) if max_restarts.get() == 2
            ),
            "{:?}",
            exited.next
        );
        {
            let guard = handle.read();
            let inst = &guard.instances()[0];
            assert_eq!(inst.state(), InstanceState::Failed);
            assert_eq!(inst.supervision().attempt, 3);
            assert_eq!(inst.supervision().next_start, None);
        }
    }

    /// The exit of a process is recorded on the slot running it only: a new
    /// instance under the id, after a stop removed the one that exited, is
    /// left alone, and so is its process group.
    #[test]
    fn an_exit_acts_only_on_the_slot_running_its_process() {
        let id = Name::new("reused-1").unwrap();
        let handle = ready_entity_with(running_under(&id, on_failure(), 0));
        let earlier_process = PID + 1;

        assert!(handle.read().runs_unclaimed_process(&id, PID));
        assert!(!handle.read().runs_unclaimed_process(&id, earlier_process));
        assert!(
            NodeEntity::mark_instance_exited(
                &handle,
                &id,
                earlier_process,
                ExitCause::signal(9, false),
                &BACKOFF,
                Instant::now(),
                || panic!("another process's exit takes no registrations"),
            )
            .is_none()
        );
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Running);

        handle.read().instances()[0].mark_stopping();
        assert!(
            !handle.read().runs_unclaimed_process(&id, PID),
            "a claimed slot's exit is its stop path's"
        );
    }

    #[test]
    fn mark_instance_exited_is_noop_for_an_instance_being_stopped() {
        let id = Name::new("stopping-1").unwrap();
        let mut instance =
            TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new());
        // A pid, so only the claim keeps the health monitor's kill away.
        instance.set_starting_process(PID, fixture_spawn_files());
        // A stop path has claimed this instance; the watcher must not relabel
        // the intentional exit as a self-exit.
        instance.mark_stopping();
        let handle = ready_entity_with(instance);

        assert!(
            NodeEntity::mark_instance_exited(
                &handle,
                &id,
                PID,
                ExitCause::Code(0),
                &BACKOFF,
                Instant::now(),
                || { panic!("a claimed instance takes no registrations") }
            )
            .is_none()
        );
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Running,
            "a stopping instance stays Running so the stop path can remove it"
        );
        assert_eq!(
            handle
                .write()
                .arm_health_kill(id.as_str(), never_answered(60)),
            None,
            "a stop path owns a claimed instance"
        );
    }

    /// A respawn starts from what the slot kept, and only from a slot that
    /// waits: a new instance's id, a slot a stop path claimed, and a slot
    /// that is not `Restarting` refuse it before any I/O.
    #[tokio::test]
    async fn prepare_and_spawn_respawn_refuses_without_a_restarting_slot() {
        let id = Name::new("waiting-1").unwrap();
        let mut waiting = waiting_instance(&id);
        waiting.supervision.restart_count = 2;
        let first_start_goal = waiting
            .supervision
            .first_start
            .as_ref()
            .map(|first| Arc::clone(&first.goal))
            .expect("a waiting slot keeps its first start");
        let handle = ready_entity_with(waiting);
        let inputs = NodeEntity::respawn_inputs_of(&handle, &id).expect("a waiting slot");
        assert_eq!(
            inputs.restart,
            on_failure(),
            "the respawn keeps the policy of the first start"
        );
        assert!(
            Arc::ptr_eq(&inputs.first_start_goal, &first_start_goal),
            "the respawn is named by the instance's own first start"
        );
        assert_eq!(inputs.restart_count, 2, "and by the restarts it has made");
        assert_eq!(
            inputs.goal,
            FirstStart::fixture(&id, "sensor", "v1").goal.for_respawn(
                BTreeMap::new(),
                BTreeMap::new(),
                Vec::new()
            )
        );

        let peppy_root = tempfile::tempdir().expect("peppy_root tempdir");
        let peppy_dirs = PeppyDirs::new(peppy_root.path());
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        fn respawn<'a>(
            instance_id: &'a Name,
            peppy_dirs: &'a PeppyDirs,
            feedback_tx: &mpsc::UnboundedSender<crate::build_io::FeedbackLine>,
        ) -> StartContext<'a> {
            struct NoHooks;
            impl OutputReaderHooks for NoHooks {}
            StartContext {
                core_node: "cn-a",
                run_log_path: std::path::Path::new("run.log"),
                instance_id,
                runtime_config_json5: "{}",
                slot_bindings: BTreeMap::new(),
                clock: Default::default(),
                launch: None,
                copy: None,
                spawn: SpawnTarget::Respawn,
                env_vars: &[],
                mount_paths_resolved: &[],
                credential: std::path::Path::new("no-such.key"),
                peppy_dirs,
                output_sinks: OutputSinks {
                    announcer: Announcer::new(scratch_log(), feedback_tx.clone()),
                    publish_enabled: Arc::new(AtomicBool::new(true)),
                    hooks: Arc::new(NoHooks),
                },
            }
        }
        let other = Name::new("other-1").unwrap();
        let error =
            NodeEntity::prepare_and_spawn(&handle, respawn(&other, &peppy_dirs, &feedback_tx))
                .await
                .expect_err("no slot waits under this id");
        assert!(
            matches!(&error, Error::RespawnTargetMissing { instance_id } if instance_id == "other-1"),
            "{error}"
        );
        // A slot a stop path claimed, and one that is `Running`, refuse too.
        handle.read().instances()[0].mark_stopping();
        let error = NodeEntity::prepare_and_spawn(&handle, respawn(&id, &peppy_dirs, &feedback_tx))
            .await
            .expect_err("a claimed slot is a stop path's");
        assert!(matches!(error, Error::RespawnTargetMissing { .. }));
        assert!(NodeEntity::respawn_inputs_of(&handle, &id).is_none());
        assert!(matches!(
            NodeEntity::claim_and_remove(&handle, &id, None),
            SlotRemoval::Removed(_)
        ));
        assert!(handle.read().instances().is_empty());
        let error = NodeEntity::prepare_and_spawn(&handle, respawn(&id, &peppy_dirs, &feedback_tx))
            .await
            .expect_err("a stopped slot is gone");
        assert!(matches!(error, Error::RespawnTargetMissing { .. }));
        assert!(NodeEntity::respawn_inputs_of(&handle, &id).is_none());
        let running = Name::new("running-1").unwrap();
        let handle = ready_entity_with(TrackedNodeInstance::new(
            running.clone(),
            InstanceState::Running,
            BTreeMap::new(),
        ));
        let error =
            NodeEntity::prepare_and_spawn(&handle, respawn(&running, &peppy_dirs, &feedback_tx))
                .await
                .expect_err("a running slot is not respawned");
        assert!(matches!(error, Error::RespawnTargetMissing { .. }));
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Running);
    }

    /// A respawn whose start fails leaves the slot waiting with no pid; one
    /// a stop path claimed meanwhile is removed, so no slot waits for a
    /// restart nobody owns.
    #[test]
    fn undo_start_reverts_a_respawn_and_removes_a_stopping_one() {
        let id = Name::new("waiting-2").unwrap();
        let handle = ready_entity_with(waiting_instance(&id));
        {
            let mut guard = handle.write();
            let NodeStage::Ready { instances, .. } = &mut guard.stage else {
                panic!("from_snapshot builds a Ready entity")
            };
            instances[0].begin_respawn();
            instances[0].set_starting_process(9, fixture_spawn_files());
        }
        assert_eq!(
            handle.read().instances()[0].state(),
            InstanceState::Starting
        );
        NodeEntity::undo_start(&handle, &id);
        {
            let guard = handle.read();
            let inst = &guard.instances()[0];
            assert_eq!(inst.state(), InstanceState::Restarting);
            assert_eq!(inst.pid(), None);
            assert_eq!(
                inst.supervision().last_exit,
                Some(ExitCause::signal(9, false)),
                "the slot keeps what it recorded"
            );
        }

        {
            let mut guard = handle.write();
            let NodeStage::Ready { instances, .. } = &mut guard.stage else {
                panic!("from_snapshot builds a Ready entity")
            };
            instances[0].begin_respawn();
            instances[0].mark_stopping();
        }
        NodeEntity::undo_start(&handle, &id);
        assert!(handle.read().instances().is_empty());
    }

    /// A failed start backs off further on a restarting slot, the cap and a
    /// refusal end its restarts, and none touches a slot a stop path claimed.
    #[test]
    fn mark_start_failed_backs_off_further_and_a_cap_or_refusal_ends_it() {
        let id = Name::new("waiting-3").unwrap();
        let handle = ready_entity_with(waiting_instance(&id));
        let before = SystemTime::now();
        let spawn_failed = ExitCause::Message("spawn failed".to_owned());
        let next = NodeEntity::mark_start_failed(
            &handle,
            &id,
            spawn_failed.clone(),
            &BACKOFF,
            Instant::now(),
        )
        .expect("a restarting slot takes the attempt");
        let Attempt::Scheduled { attempt, delay, .. } = next else {
            panic!("{next:?}")
        };
        assert_eq!(attempt.get(), 2);
        assert_eq!(delay, Duration::from_secs(4));
        {
            let guard = handle.read();
            let supervision = guard.instances()[0].supervision();
            assert_eq!(supervision.attempt, 2);
            assert!(
                supervision.next_start.expect("a start is due") >= before + delay,
                "the next start is the delay away"
            );
            assert_eq!(supervision.last_exit, Some(spawn_failed.clone()));
        }
        assert!(NodeEntity::respawn_inputs_of(&handle, &id).is_some());

        let capped_id = Name::new("waiting-4").unwrap();
        let handle = ready_entity_with(waiting_instance_under(&capped_id, capped(2)));
        assert!(matches!(
            NodeEntity::mark_start_failed(&handle, &capped_id, spawn_failed.clone(), &BACKOFF, Instant::now()),
            Some(Attempt::Scheduled { attempt, .. }) if attempt.get() == 2
        ));
        let next = NodeEntity::mark_start_failed(
            &handle,
            &capped_id,
            spawn_failed.clone(),
            &BACKOFF,
            Instant::now(),
        )
        .expect("the capped attempt is taken");
        assert!(
            matches!(
                next,
                Attempt::Capped { max_restarts, .. } if max_restarts.get() == 2
            ),
            "{next:?}"
        );
        {
            let guard = handle.read();
            let inst = &guard.instances()[0];
            assert_eq!(inst.state(), InstanceState::Failed);
            assert_eq!(inst.supervision().attempt, 3);
            assert_eq!(inst.supervision().next_start, None);
        }
        assert!(
            NodeEntity::mark_start_failed(
                &handle,
                &capped_id,
                spawn_failed.clone(),
                &BACKOFF,
                Instant::now()
            )
            .is_none(),
            "a failed slot takes no attempt"
        );

        let refused = Name::new("waiting-5").unwrap();
        let handle = ready_entity_with(waiting_instance(&refused));
        assert!(NodeEntity::mark_restart_refused(&handle, &refused));
        assert_eq!(handle.read().instances()[0].state(), InstanceState::Failed);

        let claimed = Name::new("waiting-6").unwrap();
        let handle = ready_entity_with(waiting_instance(&claimed));
        handle.read().instances()[0].mark_stopping();
        assert!(
            NodeEntity::mark_start_failed(
                &handle,
                &claimed,
                spawn_failed,
                &BACKOFF,
                Instant::now()
            )
            .is_none()
        );
        assert!(!NodeEntity::mark_restart_refused(&handle, &claimed));
        assert!(NodeEntity::respawn_inputs_of(&handle, &claimed).is_none());
    }

    /// The removal's claim removes a slot whose process it stopped, or that
    /// has none, cancelling a waiting restart with it; a `Starting` slot, a
    /// restart's start named so, and one running another process, keep their
    /// slots unclaimed.
    #[test]
    fn claim_and_remove_keeps_a_starting_slot_and_another_process() {
        let waiting = Name::new("waiting-9").unwrap();
        let starting = Name::new("starting-9").unwrap();
        let running = Name::new("running-9").unwrap();
        let restart = Name::new("restart-9").unwrap();
        let mut respawned =
            TrackedNodeInstance::new(running.clone(), InstanceState::Running, BTreeMap::new());
        respawned.set_starting_process(42, fixture_spawn_files());
        let mut restart_start = waiting_instance(&restart);
        restart_start.begin_respawn();
        let handle = ready_entity_with_all(vec![
            waiting_instance(&waiting),
            TrackedNodeInstance::new(starting.clone(), InstanceState::Starting, BTreeMap::new()),
            respawned,
            restart_start,
        ]);
        let token = handle.read().instances()[0]
            .supervision()
            .restart_token
            .clone();
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &starting, None),
            SlotRemoval::Left(SlotLeft::Starting { restart: false })
        );
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &restart, None),
            SlotRemoval::Left(SlotLeft::Starting { restart: true })
        );
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &running, Some(41)),
            SlotRemoval::Left(SlotLeft::RunsAnotherProcess),
            "a process the removal did not stop keeps its slot"
        );
        assert!(
            handle
                .read()
                .instances()
                .iter()
                .skip(1)
                .all(|inst| !inst.is_stopping()),
            "a kept slot is not claimed"
        );
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &waiting, None),
            SlotRemoval::Removed(KeptRelations::default())
        );
        assert!(token.is_cancelled(), "the waiting restart is cancelled");
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &running, Some(42)),
            SlotRemoval::Removed(KeptRelations::default()),
            "the process the removal stopped goes with its slot"
        );
        assert_eq!(handle.read().instances().len(), 2);
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &waiting, None),
            SlotRemoval::Untracked
        );
    }

    #[test]
    fn mark_stopping_cancels_the_restart_token() {
        let id = Name::new("waiting-5").unwrap();
        let instance = waiting_instance(&id);
        let token = instance.supervision().restart_token.clone();
        assert!(!token.is_cancelled());
        instance.mark_stopping();
        assert!(token.is_cancelled());
    }

    /// A respawn starts with a fresh health flag: the previous process's
    /// `unhealthy` does not carry over, and a monitor still holding the old
    /// flag writes to a detached one.
    #[test]
    fn a_respawn_starts_healthy_on_a_fresh_flag() {
        let id = Name::new("waiting-8").unwrap();
        let mut slot = waiting_instance(&id);
        slot.set_healthy(false);
        let previous = std::sync::Arc::clone(&slot.healthy);
        slot.begin_respawn();
        assert!(slot.healthy(), "the next process starts healthy");
        assert!(
            !std::sync::Arc::ptr_eq(&previous, &slot.healthy),
            "the flag is a new one"
        );
        assert!(!previous.load(std::sync::atomic::Ordering::Acquire));
    }

    /// A respawn's start reports no start due: the one it waited for is
    /// under way.
    #[test]
    fn a_respawn_reports_no_start_due() {
        let id = Name::new("waiting-8").unwrap();
        let mut slot = waiting_instance(&id);
        assert!(
            slot.restart_status().next_start_ms.is_some(),
            "a waiting instance reports its start due"
        );
        slot.begin_respawn();
        assert_eq!(slot.restart_status().next_start_ms, None);
    }

    /// An incarnation is live while its process runs, or starts once it is
    /// forked.
    #[test]
    fn only_a_running_or_forked_process_has_a_live_incarnation() {
        let id = Name::new("states-1").unwrap();
        let running = TrackedNodeInstance::new(id.clone(), InstanceState::Running, BTreeMap::new());
        assert!(running.live_incarnation().is_some(), "a running process");
        let mut starting =
            TrackedNodeInstance::new(id.clone(), InstanceState::Starting, BTreeMap::new());
        assert_eq!(
            starting.live_incarnation(),
            None,
            "a start that has not forked its process"
        );
        starting.set_starting_process(PID, fixture_spawn_files());
        assert!(starting.live_incarnation().is_some(), "a forked start");
        for state in [
            InstanceState::Restarting,
            InstanceState::Finished,
            InstanceState::Failed,
        ] {
            let instance = TrackedNodeInstance::new(id.clone(), state, BTreeMap::new());
            assert_eq!(
                instance.live_incarnation(),
                None,
                "{state:?} has no process"
            );
        }
    }

    /// Each process of a restarting instance carries its own incarnation: the
    /// exited process's is gone, the restart's is newer, and a restart that
    /// failed to start leaves none live.
    #[test]
    fn each_process_of_a_restarting_instance_has_its_own_incarnation() {
        let id = Name::new("waiting-9").unwrap();
        let mut slot = waiting_instance(&id);
        let exited = slot.incarnation;
        assert_eq!(slot.live_incarnation(), None, "a waiting slot runs nothing");
        slot.begin_respawn();
        assert_eq!(
            slot.live_incarnation(),
            None,
            "a restart has no process before its fork"
        );
        slot.set_starting_process(PID, fixture_spawn_files());
        let restarted = slot.live_incarnation().expect("a forked restart is live");
        assert!(restarted > exited, "the restart's process is newer");
        slot.revert_respawn();
        assert_eq!(
            slot.live_incarnation(),
            None,
            "a restart that failed to start runs nothing"
        );
    }

    /// The stop of a waiting instance claims and removes its slot, and hands
    /// back who hears of its end: the daemons of its remote held peers and
    /// the watchers kept for its restart.
    #[test]
    fn a_waiting_instance_is_claimed_and_removed() {
        use super::super::pairing::SlotAddr;
        let id = Name::new("waiting-6").unwrap();
        let handle = paired_entity_with(waiting_instance(&id));
        let token = handle.read().instances()[0]
            .supervision()
            .restart_token
            .clone();
        let local_peer = PlannedPair {
            own: SlotAddr::new("cn-a", "waiting-6", "arm"),
            peer: SlotAddr::new("cn-a", "commander", "arm"),
            peer_remote_meta: None,
        };
        let remote_peer = PlannedPair {
            own: SlotAddr::new("cn-a", "waiting-6", "fleet"),
            peer: SlotAddr::new("cloud", "monitor", "fleet"),
            peer_remote_meta: Some(super::super::pairing::RemoteSlotMeta {
                pairing_name: "joint_link".to_owned(),
                pairing_tag: "v1".to_owned(),
                role: "leader".to_owned(),
                cardinality: Cardinality::ZeroOrMore,
                copy: None,
            }),
        };
        for pair in [&local_peer, &remote_peer] {
            assert!(handle.write().record_held_pair(pair).is_some());
        }
        handle
            .write()
            .record_watchers_for_restart("waiting-6", &["far".to_owned()]);
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &id, None),
            SlotRemoval::Removed(KeptRelations {
                pair_daemons: std::collections::BTreeSet::from(["cloud".to_owned()]),
                watchers: vec!["far".to_owned()],
            })
        );
        assert!(token.is_cancelled(), "the stop claims the slot");
        assert!(handle.read().instances().is_empty());
        assert_eq!(
            NodeEntity::claim_and_remove(&handle, &id, None),
            SlotRemoval::Untracked
        );
    }

    /// A held pair is kept under both of its links, the instance's own and
    /// the peer's on the peer's daemon, and under no other.
    #[test]
    fn a_held_pair_is_kept_by_both_its_links() {
        use super::super::pairing::SlotAddr;
        let id = Name::new("waiting-7").unwrap();
        let handle = paired_entity_with(waiting_instance(&id));
        let own = SlotAddr::new("cn-a", "waiting-7", "arms");
        let peer = SlotAddr::new("cn-a", "commander", "arms");
        let pair = PlannedPair {
            own: own.clone(),
            peer: peer.clone(),
            peer_remote_meta: None,
        };
        assert_eq!(handle.write().record_held_pair(&pair), Some(Vec::new()));
        let guard = handle.read();
        let slot = &guard.instances()[0];
        assert!(slot.keeps_held_pair(&own, &peer));
        assert!(
            !slot.keeps_held_pair(&SlotAddr::new("cn-a", "waiting-7", "arm"), &peer),
            "another own link keeps no pair"
        );
        assert!(
            !slot.keeps_held_pair(&own, &SlotAddr::new("cn-a", "commander", "arm")),
            "another peer link keeps no pair"
        );
        assert!(
            !slot.keeps_held_pair(&own, &SlotAddr::new("cn-b", "commander", "arms")),
            "the same id and link on another daemon keep no pair"
        );
    }

    /// A pair is held once however often it is delivered, and a forgotten
    /// pair is gone; on a slot that takes one peer, a newer pair replaces
    /// the one held. The slot's cardinality comes from the node's manifest,
    /// so a slot awaiting its restart records too, and a link the node does
    /// not declare records nothing.
    #[test]
    fn held_pairs_are_recorded_once_and_a_scalar_slot_keeps_the_newest() {
        use super::super::pairing::SlotAddr;
        let id = Name::new("waiting-7").unwrap();
        let handle = paired_entity_with(waiting_instance(&id));
        let on = |link: &str, peer: &str| PlannedPair {
            own: SlotAddr::new("cn-a", "waiting-7", link),
            peer: SlotAddr::new("cn-a", peer, link),
            peer_remote_meta: None,
        };
        let held = || {
            handle.read().instances()[0]
                .supervision()
                .held_pairs
                .clone()
        };
        let pair = on("arms", "commander");
        assert_eq!(handle.write().record_held_pair(&pair), Some(Vec::new()));
        assert_eq!(handle.write().record_held_pair(&pair), Some(Vec::new()));
        assert_eq!(held(), vec![pair.clone()]);
        assert_eq!(
            handle.write().record_held_pair(&pair.mirrored()),
            None,
            "an untracked id records nothing"
        );
        assert_eq!(
            handle
                .write()
                .record_held_pair(&on("undeclared", "commander")),
            None,
            "a link the node does not declare records nothing"
        );
        assert_eq!(
            handle.write().record_held_pair(&on("arms", "second")),
            Some(Vec::new()),
            "a set slot displaces nothing"
        );
        assert_eq!(
            held(),
            vec![pair.clone(), on("arms", "second")],
            "a set slot holds both"
        );
        assert_eq!(
            handle.write().record_held_pair(&on("arm", "first")),
            Some(Vec::new())
        );
        assert_eq!(
            handle.write().record_held_pair(&on("arm", "newest")),
            Some(vec![on("arm", "first")]),
            "a scalar slot's newest pair displaces the one it held"
        );
        assert_eq!(
            handle.write().record_held_pair(&on("arm", "newest")),
            Some(Vec::new()),
            "a scalar slot's pair recorded again, as a restart makes it again, displaces nothing"
        );
        assert_eq!(
            held(),
            vec![pair.clone(), on("arms", "second"), on("arm", "newest")],
            "a scalar slot holds the newest, and the pairs of other slots stay"
        );
        handle.write().forget_held_pair(&on("arm", "newest"));
        handle.write().forget_held_pair(&pair);
        assert_eq!(held(), vec![on("arms", "second")]);
        handle.write().forget_held_pair(&on("arms", "second"));
        assert!(held().is_empty());
    }

    /// A shrunk observer set and a rewritten watcher list land on a
    /// restarting instance's kept registrations, which its respawn's goal
    /// carries; a running instance takes them live.
    #[test]
    fn a_restarting_slot_keeps_the_observer_sets_and_watchers_written_to_it() {
        let id = Name::new("waiting-7").unwrap();
        let handle = ready_entity_with(waiting_instance(&id));
        let sources = ObservationTargets::new(
            "sources",
            vec![core_node_api::encoding::ObservationTarget::new(
                "first", "control", "cn-a",
            )],
        )
        .expect("one member");
        let slots: BTreeMap<String, ObservationTargets> = [("sources".to_owned(), sources.clone())]
            .into_iter()
            .collect();
        assert!(
            handle
                .write()
                .record_observations_for_restart("waiting-7", &slots)
        );
        assert_eq!(
            handle.read().instances()[0]
                .supervision()
                .registrations
                .observations,
            slots
        );
        // A later record of another slot keeps the first one's.
        let fleet = ObservationTargets::new(
            "fleet",
            vec![core_node_api::encoding::ObservationTarget::new(
                "second", "joints", "cn-a",
            )],
        )
        .expect("one member");
        let fleet_only: BTreeMap<String, ObservationTargets> =
            [("fleet".to_owned(), fleet.clone())].into_iter().collect();
        assert!(
            handle
                .write()
                .record_observations_for_restart("waiting-7", &fleet_only)
        );
        let both: BTreeMap<String, ObservationTargets> = [
            ("sources".to_owned(), sources.clone()),
            ("fleet".to_owned(), fleet),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            handle.read().instances()[0]
                .supervision()
                .registrations
                .observations,
            both,
            "the record upserts by slot"
        );
        let watchers = vec!["cn-b".to_owned(), "cn-c".to_owned()];
        handle
            .write()
            .record_watchers_for_restart("waiting-7", &watchers);
        assert_eq!(
            handle.read().instances()[0]
                .supervision()
                .registrations
                .watchers,
            watchers
        );
        let goal = NodeEntity::respawn_inputs_of(&handle, &id)
            .expect("a waiting slot")
            .goal;
        assert_eq!(goal.planned_observations, both);
        assert_eq!(goal.lifecycle_watchers, watchers);
        let running = Name::new("running-7").unwrap();
        let handle = ready_entity_with(TrackedNodeInstance::new(
            running.clone(),
            InstanceState::Running,
            BTreeMap::new(),
        ));
        assert!(
            !handle
                .write()
                .record_observations_for_restart("running-7", &slots),
            "a running instance takes its sets live"
        );
        handle
            .write()
            .record_watchers_for_restart("running-7", &watchers);
        assert!(
            handle.read().instances()[0]
                .supervision()
                .registrations
                .watchers
                .is_empty(),
            "a running instance's watchers live in the registry"
        );
    }

    /// A deliberate stop of an instance forgets the pairs its peers held
    /// with it, on every instance; a pair with another peer and a pair with
    /// a peer of the same id on another daemon stay.
    #[test]
    fn forget_pairs_held_with_keeps_other_peers() {
        use super::super::pairing::{RemoteSlotMeta, SlotAddr};
        let first = Name::new("first-8").unwrap();
        let second = Name::new("second-8").unwrap();
        let handle = entity_of(
            paired_config(),
            vec![waiting_instance(&first), waiting_instance(&second)],
        );
        let with = |own: &str, peer: &str, remote: bool| PlannedPair {
            own: SlotAddr::new("cn-a", own, "arms"),
            peer: SlotAddr::new(if remote { "cn-b" } else { "cn-a" }, peer, "arms"),
            peer_remote_meta: remote.then(|| RemoteSlotMeta {
                pairing_name: "arm_link".to_owned(),
                pairing_tag: "v1".to_owned(),
                role: "arm".to_owned(),
                cardinality: config::node::Cardinality::ZeroOrOne,
                copy: None,
            }),
        };
        for own in ["first-8", "second-8"] {
            for pair in [
                with(own, "ctrl", false),
                with(own, "other", false),
                with(own, "ctrl", true),
            ] {
                assert!(handle.write().record_held_pair(&pair).is_some());
            }
        }
        handle.write().forget_pairs_held_with("cn-a", "ctrl");
        for (index, own) in ["first-8", "second-8"].into_iter().enumerate() {
            assert_eq!(
                handle.read().instances()[index].supervision().held_pairs,
                vec![with(own, "other", false), with(own, "ctrl", true)],
                "{own}"
            );
        }
        handle.write().forget_pairs_held_with("cn-b", "ctrl");
        for (index, own) in ["first-8", "second-8"].into_iter().enumerate() {
            assert_eq!(
                handle.read().instances()[index].supervision().held_pairs,
                vec![with(own, "other", false)],
                "{own}: a remote peer's stop forgets it too"
            );
        }
    }

    /// The duplicate-id refusal names the state and, for a waiting slot, the
    /// stop that frees the id.
    #[test]
    fn duplicate_instance_id_names_a_restarting_state() {
        let error = Error::DuplicateInstanceId {
            instance_id: "arm_inst".to_owned(),
            node_name: "arm".to_owned(),
            node_tag: "v1".to_owned(),
            core_node: "cn-a".to_owned(),
            state: InstanceState::Restarting,
        };
        assert_eq!(
            error.to_string(),
            "Instance ID `arm_inst` already exists for node `arm:v1` and is restarting; \
             `peppy node stop arm_inst --core-node cn-a` frees the id"
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
                InstanceState::Finished,
                BTreeMap::new(),
            )],
        );
        assert!(
            entity.stop_instance(&id).is_some(),
            "terminal instance is removable"
        );
        assert!(entity.instances().is_empty());
    }
}
