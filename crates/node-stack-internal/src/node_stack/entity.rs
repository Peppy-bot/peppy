use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex, RwLock};

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
            instance_ids: entity
                .instances()
                .iter()
                .map(|i| i.instance_id().as_str().to_string())
                .collect(),
        }
    }
}

/// Lifecycle stage of a `NodeEntity`. Each variant carries every piece of state
/// reached so far — once an entity progresses past `Added`, the original
/// `config_path` is still available; once past `Built`, the `artifact_path` is too.
///
/// `Building` and `Starting` are explicit "in progress" stages that act as the
/// concurrency barrier: a second concurrent `build` (or `prepare_and_spawn`) on
/// the same entity sees these stages and is rejected immediately, with no
/// queueing or async wait.
#[derive(Debug, Clone)]
pub enum NodeStage {
    Added {
        config_path: PathBuf,
    },
    Building {
        config_path: PathBuf,
    },
    Built {
        config_path: PathBuf,
        artifact_path: PathBuf,
    },
    Starting {
        config_path: PathBuf,
        artifact_path: PathBuf,
        /// Instances that already existed before `prepare_and_spawn` was
        /// called. Empty when transitioning from `Built`; non-empty when
        /// transitioning from `Started` (i.e. starting a *second* / Nth
        /// instance of an already-running node). On commit, the new instance
        /// is appended to this list. On abort, the entity rolls back to
        /// `Built` (if empty) or `Started` (if non-empty), preserving the
        /// previously-running instances.
        prior_instances: Vec<TrackedNodeInstance>,
    },
    Started {
        config_path: PathBuf,
        artifact_path: PathBuf,
        instances: Vec<TrackedNodeInstance>,
    },
}

impl NodeStage {
    fn name(&self) -> &'static str {
        match self {
            NodeStage::Added { .. } => "Added",
            NodeStage::Building { .. } => "Building",
            NodeStage::Built { .. } => "Built",
            NodeStage::Starting { .. } => "Starting",
            NodeStage::Started { .. } => "Started",
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
    /// Channel that receives stdout/stderr lines from the running child. The
    /// reader tasks remain alive past `prepare_and_spawn`'s return.
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
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
    pub stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
    pub output_reader_handles: Vec<JoinHandle<()>>,
    pub log_file: Arc<StdMutex<File>>,
}

#[derive(Clone, Debug)]
pub struct NodeEntity {
    config: NodeConfig,
    stage: NodeStage,
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
        }
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
            NodeStage::Built { config_path, .. } => config_path,
            NodeStage::Starting { config_path, .. } => config_path,
            NodeStage::Started { config_path, .. } => config_path,
        }
    }

    /// Returns the path to the built `.sif`/archive in
    /// `~/.peppy/added_nodes`. `None` until the entity has reached `Built`.
    pub fn artifact_path(&self) -> Option<&Path> {
        match &self.stage {
            NodeStage::Added { .. } | NodeStage::Building { .. } => None,
            NodeStage::Built { artifact_path, .. } => Some(artifact_path),
            NodeStage::Starting { artifact_path, .. } => Some(artifact_path),
            NodeStage::Started { artifact_path, .. } => Some(artifact_path),
        }
    }

    /// Returns the running instances of this entity. For `Started` returns
    /// the registered instances. For `Starting` returns the *prior* instances
    /// (those that were already running before the in-flight start) so that
    /// external observers don't see existing instances disappear during the
    /// brief window where a new instance is being spawned. Returns an empty
    /// slice for `Added`, `Building`, and `Built`.
    pub fn instances(&self) -> &[TrackedNodeInstance] {
        match &self.stage {
            NodeStage::Started { instances, .. } => instances,
            NodeStage::Starting {
                prior_instances, ..
            } => prior_instances,
            _ => &[],
        }
    }

    /// Performs the actual `.sif`/archive build for the entity behind `handle`
    /// and transitions its stage `Added → Building → Built` (or rolls back to
    /// `Added` on failure).
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
    /// transition is allowed until either success (`Built`) or failure
    /// (rolled back to `Added`).
    ///
    /// Returns [`Error::InvalidStageTransition`] if the entity is not in
    /// [`NodeStage::Added`], or [`Error::BuildFailed`] if the underlying
    /// `add_cmd` / apptainer / archive step fails.
    pub async fn build(handle: &Arc<RwLock<NodeEntity>>, ctx: BuildContext<'_>) -> Result<()> {
        // ---- Phase 1: Added → Building, snapshot inputs (brief write lock) ----
        let (node_name, node_tag, config_path, container_opt, add_cmd) = {
            let mut guard = handle.write().expect("entity poisoned");
            let config_path = match &guard.stage {
                NodeStage::Added { config_path } => config_path.clone(),
                other => {
                    return Err(Error::InvalidStageTransition {
                        node_name: guard.config.manifest.name.as_str().to_owned(),
                        node_tag: guard.config.manifest.tag.clone(),
                        from: other.name(),
                        to: "Built",
                    });
                }
            };
            let snapshot = (
                guard.config.manifest.name.as_str().to_owned(),
                guard.config.manifest.tag.clone(),
                config_path.clone(),
                guard.config.execution.container.clone(),
                guard.config.execution.add_cmd.clone(),
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
        let mut guard = handle.write().expect("entity poisoned");
        // A concurrent `push_config` could have replaced the entity wholesale
        // while we were running I/O. If we are no longer the in-flight build,
        // silently discard the result rather than clobbering the new state.
        if !matches!(guard.stage, NodeStage::Building { .. }) {
            return Err(Error::InvalidStageTransition {
                node_name,
                node_tag,
                from: guard.stage.name(),
                to: "Built",
            });
        }

        match io_result {
            Ok(()) => {
                let artifact_path = if is_container {
                    move_sif_to_storage(ctx.working_dir, &node_name, &node_tag, ctx.peppy_dirs)
                        .map_err(|e| {
                            // Rollback Building → Added on storage failure too.
                            guard.stage = NodeStage::Added {
                                config_path: config_path.clone(),
                            };
                            Error::BuildFailed {
                                node_name: node_name.clone(),
                                node_tag: node_tag.clone(),
                                reason: format!("failed to move container image to storage: {}", e),
                            }
                        })?
                } else {
                    archive_dir_to_storage(ctx.working_dir, &node_name, &node_tag, ctx.peppy_dirs)
                        .map_err(|e| {
                        guard.stage = NodeStage::Added {
                            config_path: config_path.clone(),
                        };
                        Error::BuildFailed {
                            node_name: node_name.clone(),
                            node_tag: node_tag.clone(),
                            reason: format!("failed to archive node directory: {}", e),
                        }
                    })?
                };

                guard.stage = NodeStage::Built {
                    config_path,
                    artifact_path,
                };
                Ok(())
            }
            Err(e) => {
                // Roll back Building → Added so the entity can be retried.
                guard.stage = NodeStage::Added { config_path };
                Err(e)
            }
        }
    }

    /// Best-effort rollback from `Starting`. Used by both [`prepare_and_spawn`]
    /// (on I/O failure during phases 2–4) and [`abort_started`] (on
    /// caller-side ready/health failure).
    ///
    /// If the `Starting` stage carries no `prior_instances`, rolls back to
    /// `Built`. If it carries `prior_instances` (i.e. we were starting an
    /// additional instance of an already-running node), rolls back to
    /// `Started` with those prior instances preserved — the existing
    /// instances are unaffected by the failed start of the new one.
    ///
    /// Skips the rollback if a concurrent `push_config` replaced the entity
    /// wholesale — the new state takes precedence in that case.
    fn try_rollback_starting(handle: &Arc<RwLock<NodeEntity>>) {
        let mut guard = handle.write().expect("entity poisoned");
        if !matches!(guard.stage, NodeStage::Starting { .. }) {
            return;
        }
        let placeholder = NodeStage::Added {
            config_path: PathBuf::new(),
        };
        if let NodeStage::Starting {
            config_path,
            artifact_path,
            prior_instances,
        } = std::mem::replace(&mut guard.stage, placeholder)
        {
            guard.stage = if prior_instances.is_empty() {
                NodeStage::Built {
                    config_path,
                    artifact_path,
                }
            } else {
                NodeStage::Started {
                    config_path,
                    artifact_path,
                    instances: prior_instances,
                }
            };
        }
    }

    /// Phase 1 of the start lifecycle: validates the entity is in `Built` or
    /// `Started`, transitions it to `Starting`, prepares the instance
    /// directory, spawns the child process, and wires up output streaming.
    /// Returns the spawned `Child` along with a [`StartedInstanceCtx`] that
    /// the caller must hand back to either [`NodeEntity::commit_started`]
    /// (success) or [`NodeEntity::abort_started`] (failure).
    ///
    /// Allowed source stages:
    /// - `Built` → first instance of this node (Starting carries no prior
    ///   instances).
    /// - `Started` → additional (Nth) instance of an already-running node
    ///   (Starting carries the existing instances forward, so they remain
    ///   visible to external observers via [`Self::instances`] and survive a
    ///   failed start of the new instance).
    ///
    /// Concurrency: a second concurrent `prepare_and_spawn` on the same
    /// entity observes the `Starting` stage and is rejected immediately with
    /// [`Error::InvalidStageTransition`]. Only one start can be in flight
    /// at a time per entity, regardless of how many instances are already
    /// running.
    ///
    /// Returns [`Error::DuplicateInstanceId`] if `ctx.instance_id` is already
    /// tracked by the entity.
    ///
    /// On any I/O failure inside this function, the entity is rolled back to
    /// its prior stage (`Built` or `Started` with the prior instances) before
    /// returning.
    pub async fn prepare_and_spawn(
        handle: &Arc<RwLock<NodeEntity>>,
        ctx: StartContext<'_>,
    ) -> Result<(Child, StartedInstanceCtx)> {
        // ---- Phase 1: {Built, Started} → Starting, snapshot (brief write lock) ----
        let (node_name, node_tag, node_config, artifact_path) = {
            let mut guard = handle.write().expect("entity poisoned");
            // Move out of the current stage so we can take ownership of its
            // fields without holding multiple borrows. We swap in a temporary
            // Added placeholder and either replace it with Starting (success)
            // or restore the original stage (failure).
            let placeholder = NodeStage::Added {
                config_path: PathBuf::new(),
            };
            let original = std::mem::replace(&mut guard.stage, placeholder);
            let (config_path, artifact_path, prior_instances) = match original {
                NodeStage::Built {
                    config_path,
                    artifact_path,
                } => (config_path, artifact_path, Vec::new()),
                NodeStage::Started {
                    config_path,
                    artifact_path,
                    instances,
                } => {
                    // Reject duplicate instance ids early — before any I/O.
                    if instances
                        .iter()
                        .any(|inst| inst.instance_id() == ctx.instance_id)
                    {
                        let err = Error::DuplicateInstanceId {
                            instance_id: ctx.instance_id.as_str().to_owned(),
                            node_name: guard.config.manifest.name.as_str().to_owned(),
                            node_tag: guard.config.manifest.tag.clone(),
                        };
                        // Restore the original stage before returning.
                        guard.stage = NodeStage::Started {
                            config_path,
                            artifact_path,
                            instances,
                        };
                        return Err(err);
                    }
                    (config_path, artifact_path, instances)
                }
                other => {
                    let from = other.name();
                    let err = Error::InvalidStageTransition {
                        node_name: guard.config.manifest.name.as_str().to_owned(),
                        node_tag: guard.config.manifest.tag.clone(),
                        from,
                        to: "Started",
                    };
                    // Restore the original stage before returning.
                    guard.stage = other;
                    return Err(err);
                }
            };
            let snapshot = (
                guard.config.manifest.name.as_str().to_owned(),
                guard.config.manifest.tag.clone(),
                guard.config.clone(),
                artifact_path.clone(),
            );
            // Atomic transition into Starting. Any second concurrent call
            // now sees Starting.
            guard.stage = NodeStage::Starting {
                config_path,
                artifact_path,
                prior_instances,
            };
            snapshot
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
            Self::try_rollback_starting(handle);
            Error::StartFailed {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                reason,
            }
        })?;

        // ---- Phase 3: spawn the child process ----
        let mut child = if let Some(container) = node_config.execution.container.as_ref() {
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
                &ctx.log_file,
                ctx.peppy_dirs,
            )
            .await
        } else {
            spawn_process_node(
                &node_config,
                &instance_dir,
                ctx.runtime_config_json5,
                ctx.env_vars,
                &ctx.log_file,
                ctx.peppy_dirs,
            )
        }
        .map_err(|e| {
            Self::try_rollback_starting(handle);
            Error::StartFailed {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                reason: format!("failed to spawn child: {}", e),
            }
        })?;

        // ---- Phase 4: wire output streaming ----
        let stderr_buffer = Arc::new(StdMutex::new(VecDeque::new()));
        let mut output_reader_handles = Vec::new();

        if let Some(stdout) = child.stdout.take() {
            output_reader_handles.push(spawn_output_reader_async(
                stdout,
                ctx.feedback_tx.clone(),
                Arc::clone(&ctx.publish_enabled),
                Arc::clone(&ctx.hooks),
                FeedbackStream::Stdout,
                None,
                Arc::clone(&ctx.log_file),
            ));
        }

        if let Some(stderr) = child.stderr.take() {
            output_reader_handles.push(spawn_output_reader_async(
                stderr,
                ctx.feedback_tx.clone(),
                Arc::clone(&ctx.publish_enabled),
                Arc::clone(&ctx.hooks),
                FeedbackStream::Stderr,
                Some(Arc::clone(&stderr_buffer)),
                Arc::clone(&ctx.log_file),
            ));
        }

        Ok((
            child,
            StartedInstanceCtx {
                instance_dir,
                stderr_buffer,
                output_reader_handles,
                log_file: ctx.log_file,
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
    /// [`Error::InvalidStageTransition`] without disturbing the new state.
    /// The caller is responsible for cleaning up the still-running child in
    /// that case (e.g. by calling `child.kill()` after the error).
    pub async fn commit_started(
        handle: &Arc<RwLock<NodeEntity>>,
        child: Child,
        _started_ctx: StartedInstanceCtx,
        instance_id: Name,
    ) -> Result<u32> {
        let pid = child.id().unwrap_or(0);
        // Drop the Child to release the tokio handle. tokio::process::Child
        // does NOT have kill_on_drop unless explicitly configured (and we
        // don't), so the OS process keeps running. The daemon will manage
        // termination later via PID polling in stop_instance.
        drop(child);

        let mut guard = handle.write().expect("entity poisoned");
        // Verify we're still in Starting (a concurrent push_config could have
        // replaced the entity), then take ownership of its fields so we can
        // append the new instance to the prior_instances list.
        if !matches!(guard.stage, NodeStage::Starting { .. }) {
            return Err(Error::InvalidStageTransition {
                node_name: guard.config.manifest.name.as_str().to_owned(),
                node_tag: guard.config.manifest.tag.clone(),
                from: guard.stage.name(),
                to: "Started",
            });
        }
        let placeholder = NodeStage::Added {
            config_path: PathBuf::new(),
        };
        let (config_path, artifact_path, mut instances) =
            match std::mem::replace(&mut guard.stage, placeholder) {
                NodeStage::Starting {
                    config_path,
                    artifact_path,
                    prior_instances,
                } => (config_path, artifact_path, prior_instances),
                _ => unreachable!("matched Starting above"),
            };

        let tracked = TrackedNodeInstance::new(instance_id, Some(pid));
        instances.push(tracked);
        guard.stage = NodeStage::Started {
            config_path,
            artifact_path,
            instances,
        };

        Ok(pid)
    }

    /// Phase 2 (failure): kills the spawned child, joins the reader tasks (so
    /// the stderr buffer flushes), rolls the entity back from `Starting` to
    /// its prior stage (`Built` if no prior instances, `Started` if there
    /// were prior instances), and returns a formatted error message
    /// including a stderr tail.
    ///
    /// If a concurrent `push_config` replaced the entity wholesale while the
    /// daemon was running its messenger checks, the rollback is silently
    /// skipped — the new state takes precedence. The child is still killed
    /// either way.
    pub async fn abort_started(
        handle: &Arc<RwLock<NodeEntity>>,
        child: Child,
        started_ctx: StartedInstanceCtx,
        error: String,
        instance_id: &Name,
    ) -> String {
        let msg = kill_and_collect_error(
            child,
            instance_id.as_str(),
            &error,
            started_ctx.stderr_buffer,
            started_ctx.output_reader_handles,
            started_ctx.log_file,
        )
        .await;

        // Best-effort rollback Starting → Built. Skip if a concurrent
        // push_config replaced the entity in the meantime.
        Self::try_rollback_starting(handle);

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
            stage: NodeStage::Started {
                config_path: root_path.clone(),
                artifact_path: root_path,
                instances: vec![instance],
            },
        }
    }

    /// Reconstructs an entity from a previously-captured snapshot during stack
    /// restore. The state is taken at face value — no I/O is performed and
    /// the lifecycle is bypassed.
    ///
    /// Used only by [`crate::node_stack::NodeStack::apply_from`] when cloning
    /// state from another stack at startup, where the artifact already exists
    /// on disk and the source instances are still tracked. The resulting stage
    /// is determined by the `(artifact_path, instances)` combination:
    ///
    /// - `(None, [])` → `Added`
    /// - `(Some, [])` → `Built`
    /// - `(Some, instances)` → `Started`
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
                 a node cannot be Started without first being Built"
            ),
            (Some(artifact_path), true) => NodeStage::Built {
                config_path,
                artifact_path,
            },
            (Some(artifact_path), false) => NodeStage::Started {
                config_path,
                artifact_path,
                instances,
            },
        };
        Self { config, stage }
    }

    /// Test-only: directly inject a stage for transition-rejection tests and
    /// fixtures. Production code must drive transitions via `build`,
    /// `prepare_and_spawn`, etc.
    #[cfg(any(test, feature = "test_helpers"))]
    pub fn __test_set_stage(&mut self, stage: NodeStage) {
        self.stage = stage;
    }

    /// Records a freshly-spawned instance and transitions `Built → Started`
    /// (or appends to the existing `Started` vec).
    ///
    /// Returns [`Error::DuplicateInstanceId`] if the instance id is already
    /// tracked, or [`Error::InvalidStageTransition`] if the entity has not
    /// yet been built.
    pub fn start_instance(&mut self, instance: TrackedNodeInstance) -> Result<()> {
        match &mut self.stage {
            NodeStage::Built { .. } => {
                // Move out of the current stage to take ownership of the paths.
                let (config_path, artifact_path) = match std::mem::replace(
                    &mut self.stage,
                    // Temporary placeholder; immediately overwritten below.
                    NodeStage::Added {
                        config_path: PathBuf::new(),
                    },
                ) {
                    NodeStage::Built {
                        config_path,
                        artifact_path,
                    } => (config_path, artifact_path),
                    _ => unreachable!("matched Built above"),
                };
                self.stage = NodeStage::Started {
                    config_path,
                    artifact_path,
                    instances: vec![instance],
                };
                Ok(())
            }
            NodeStage::Started { instances, .. } => {
                if instances
                    .iter()
                    .any(|inst| inst.instance_id() == instance.instance_id())
                {
                    return Err(Error::DuplicateInstanceId {
                        instance_id: instance.instance_id().as_str().to_owned(),
                        node_name: self.config.manifest.name.as_str().to_owned(),
                        node_tag: self.config.manifest.tag.clone(),
                    });
                }
                instances.push(instance);
                Ok(())
            }
            other => Err(Error::InvalidStageTransition {
                node_name: self.config.manifest.name.as_str().to_owned(),
                node_tag: self.config.manifest.tag.clone(),
                from: other.name(),
                to: "Started",
            }),
        }
    }

    /// Removes the matching instance from a `Started` entity. If the resulting
    /// instance vec is empty, the entity falls back to `Built` (preserving
    /// `config_path` and `artifact_path`).
    ///
    /// Returns `true` if an instance was removed, `false` if the instance id
    /// was not found or the entity is not in `Started`.
    pub fn stop_instance(&mut self, instance_id: &Name) -> bool {
        let NodeStage::Started { instances, .. } = &mut self.stage else {
            return false;
        };

        let Some(pos) = instances
            .iter()
            .position(|inst| inst.instance_id() == instance_id)
        else {
            return false;
        };

        instances.remove(pos);

        if instances.is_empty() {
            // Transition Started → Built, preserving the paths.
            let placeholder = NodeStage::Added {
                config_path: PathBuf::new(),
            };
            if let NodeStage::Started {
                config_path,
                artifact_path,
                ..
            } = std::mem::replace(&mut self.stage, placeholder)
            {
                self.stage = NodeStage::Built {
                    config_path,
                    artifact_path,
                };
            }
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
}

impl TrackedNodeInstance {
    pub fn new(instance_id: Name, pid: Option<u32>) -> Self {
        Self { instance_id, pid }
    }

    pub fn instance_id(&self) -> &Name {
        &self.instance_id
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencySpec {
    pub node_name: String,
    pub node_tag: String,
}
