use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use config::consts::PeppyDirs;
use config::node::{Name, NodeConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::build_io::FeedbackLine;
use crate::error::{Error, Result};

use super::build_steps::{
    ContainerBuildInputs, archive_dir_to_storage, build_container_image, move_sif_to_storage,
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
#[derive(Debug, Clone)]
pub enum NodeStage {
    Added {
        config_path: PathBuf,
    },
    Built {
        config_path: PathBuf,
        artifact_path: PathBuf,
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
            NodeStage::Built { .. } => "Built",
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
    /// container nodes) the apptainer `.def` file. The build artifact is
    /// produced inside this directory and then moved to peppy storage.
    pub working_dir: &'a Path,
    /// Resolved peppy directory layout. The built `.sif`/archive is placed
    /// inside `peppy_dirs.added_nodes_dir()`.
    pub peppy_dirs: &'a PeppyDirs,
    /// Channel that streams stdout/stderr lines from the build child process
    /// back to the caller (e.g. the action goal handler).
    pub feedback_tx: &'a mpsc::UnboundedSender<FeedbackLine>,
    /// Log file the build output is also written to.
    pub log_file: Arc<StdMutex<File>>,
}

#[derive(Clone, Debug)]
pub struct NodeEntity {
    config: NodeConfig,
    stage: NodeStage,
    /// Per-entity async mutex used to serialize concurrent
    /// [`NodeEntity::build`] calls. Wrapped in `Arc` so that clones of the
    /// entity (and the `Arc<RwLock<NodeEntity>>` handles handed out by the
    /// stack) all share the same lock.
    build_lock: Arc<tokio::sync::Mutex<()>>,
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
            build_lock: Arc::new(tokio::sync::Mutex::new(())),
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
            NodeStage::Built { config_path, .. } => config_path,
            NodeStage::Started { config_path, .. } => config_path,
        }
    }

    /// Returns the path to the built `.sif`/archive in
    /// `~/.peppy/added_nodes`. `None` until the entity has reached `Built`.
    pub fn artifact_path(&self) -> Option<&Path> {
        match &self.stage {
            NodeStage::Added { .. } => None,
            NodeStage::Built { artifact_path, .. } => Some(artifact_path),
            NodeStage::Started { artifact_path, .. } => Some(artifact_path),
        }
    }

    /// Returns the running instances of this entity. Returns an empty slice
    /// for non-`Started` stages.
    pub fn instances(&self) -> &[TrackedNodeInstance] {
        match &self.stage {
            NodeStage::Started { instances, .. } => instances,
            _ => &[],
        }
    }

    /// Performs the actual `.sif`/archive build for the entity behind `handle`
    /// and transitions its stage `Added → Built`.
    ///
    /// For container nodes, this runs `apptainer build` and moves the resulting
    /// `.sif` into `peppy_dirs.added_nodes_dir()`. For process nodes, this
    /// archives the working directory into a `.tar.zst` in the same location.
    /// In the process-node case, the caller is expected to have already run
    /// any user-defined `add_cmd` against `working_dir` before calling `build`.
    ///
    /// `build` is implemented as an associated function (rather than a `&mut
    /// self` method) so that no lock is held across the apptainer / archive
    /// `.await`: a brief read lock extracts the inputs from the entity, the
    /// I/O runs lock-free, and a brief write lock applies the stage
    /// transition.
    ///
    /// Returns [`Error::InvalidStageTransition`] if the entity is not in
    /// [`NodeStage::Added`], or [`Error::BuildFailed`] if the underlying
    /// apptainer/archive step fails.
    pub async fn build(handle: &Arc<RwLock<NodeEntity>>, ctx: BuildContext<'_>) -> Result<()> {
        // ---- Serialize concurrent builds against the same entity ----
        // Clone the per-entity build lock out from under a brief read lock so
        // we can `.await` on it without holding the `RwLock`. Held for the
        // entire build; a queued second caller will fall through to the
        // stage check below and observe `Built`, returning
        // `InvalidStageTransition` — the desired serialized behavior.
        let build_lock = handle.read().expect("entity poisoned").build_lock.clone();
        let _build_guard = build_lock.lock().await;

        // ---- Read phase: extract everything we need under a brief read lock ----
        let (node_name, node_tag, config_path, container_opt) = {
            let guard = handle.read().expect("entity poisoned");
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
            (
                guard.config.manifest.name.as_str().to_owned(),
                guard.config.manifest.tag.clone(),
                config_path,
                guard.config.execution.container.clone(),
            )
        };

        // ---- I/O phase: no entity lock held while apptainer runs ----
        // For container nodes, build the .sif inside `working_dir`. For both
        // node kinds, defer publishing the artifact into shared storage until
        // *after* we re-confirm the entity is still `Added` under the write
        // lock — otherwise a stale build could orphan/overwrite an artifact
        // installed by a competing winner.
        let is_container = container_opt.is_some();
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
        }

        // ---- Write phase: apply the transition under a brief write lock ----
        // The per-entity `build_lock` already serializes builds against this
        // entity, and the storage move/archive below is fast filesystem I/O,
        // so it is safe to perform under the entity write lock here.
        let mut guard = handle.write().expect("entity poisoned");
        if !matches!(guard.stage, NodeStage::Added { .. }) {
            // Someone mutated the entity while we were building. Surface this
            // as a transition error rather than overwriting the new stage. We
            // have not yet published any artifact into shared storage, so
            // there is nothing to clean up.
            return Err(Error::InvalidStageTransition {
                node_name,
                node_tag,
                from: guard.stage.name(),
                to: "Built",
            });
        }

        let artifact_path =
            if is_container {
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

        guard.stage = NodeStage::Built {
            config_path,
            artifact_path,
        };
        Ok(())
    }

    /// Forces the entity into the `Built` stage with the given `artifact_path`,
    /// without performing any I/O.
    ///
    /// **Escape hatch — prefer [`NodeEntity::build`] in production code.**
    /// This bypass exists for three narrow cases:
    /// - The daemon's root entity (which represents the running daemon and is
    ///   not a buildable node).
    /// - [`crate::node_stack::NodeStack::apply_from`], which clones state
    ///   from another stack at startup when the artifact already exists on
    ///   disk.
    /// - Tests that need to set up entities in `Built`/`Started` without
    ///   actually invoking apptainer or archiving.
    pub fn restore_built(&mut self, artifact_path: PathBuf) -> Result<()> {
        let config_path = match &self.stage {
            NodeStage::Added { config_path } => config_path.clone(),
            other => {
                return Err(Error::InvalidStageTransition {
                    node_name: self.config.manifest.name.as_str().to_owned(),
                    node_tag: self.config.manifest.tag.clone(),
                    from: other.name(),
                    to: "Built",
                });
            }
        };
        self.stage = NodeStage::Built {
            config_path,
            artifact_path,
        };
        Ok(())
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
