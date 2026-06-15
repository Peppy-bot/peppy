pub mod add_steps;
mod build_steps;
mod entity;
mod run_steps;

pub use entity::{
    BuildContext, NodeEntity, NodeStage, OutputSinks, StartContext, StartedInstanceCtx,
    TrackedNodeInstance, WorkingDirGuard,
};

use crate::error::{Error, Result};
use crate::service_action_cycle::{CycleCheckNode, find_service_action_cycle};
use config::node::{
    Name, NodeConfig, collect_dependency_specs, collect_interface_conformance_edges,
    validate_dependency_specs,
};
use core_node_api::{InstanceState, SerializedEdge, SerializedNode, SerializedNodeGraph};
use names_generator2::get_random;
use parking_lot::RwLock;
use petgraph::{
    Direction,
    dot::{Config, Dot},
    stable_graph::{NodeIndex, StableDiGraph},
    visit::EdgeRef,
};
use rand::rng;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

/// Shared handle to a `NodeEntity` stored inside a `NodeStack`. All readers
/// and writers go through the inner `RwLock`; the same `Arc` is held by the
/// graph and by every external caller, so mutations through `find()` are
/// reflected in subsequent stack queries without any take-and-replace dance.
pub type EntityHandle = Arc<RwLock<NodeEntity>>;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct NodeKey {
    name: String,
    tag: String,
}

impl NodeKey {
    fn new(name: &str, tag: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            tag: tag.trim().to_owned(),
        }
    }
}

fn key_from_entity(entity: &NodeEntity) -> NodeKey {
    NodeKey::new(
        entity.config().manifest.name.as_str(),
        &entity.config().manifest.tag,
    )
}

fn dependency_keys(node: &NodeConfig) -> Vec<NodeKey> {
    collect_dependency_specs(node)
        .into_iter()
        .map(|spec| NodeKey::new(&spec.node_name, &spec.node_tag))
        .collect()
}

/// Whether `inst` is the instance addressed by `instance_id`. When
/// `require_running` is set, only `Running` instances match; otherwise an
/// instance in any state (e.g. `Starting`) matches.
fn instance_matches(inst: &TrackedNodeInstance, instance_id: &Name, require_running: bool) -> bool {
    inst.instance_id() == instance_id
        && (!require_running || inst.state() == InstanceState::Running)
}

/// Builds the trimmed `(name, tag)` key used by the `add_log_paths` cache.
/// Trimming matches [`NodeKey::new`] so a log path stored under one spelling
/// of the key is found again regardless of incidental surrounding whitespace.
fn add_log_key(name: &str, tag: &str) -> (String, String) {
    (name.trim().to_owned(), tag.trim().to_owned())
}

struct NodeStackInner {
    graph: StableDiGraph<EntityHandle, ()>,
    key_to_index: HashMap<NodeKey, NodeIndex>,
    pending_requirements: HashMap<NodeKey, Vec<NodeIndex>>,
    root_key: NodeKey,
}

impl NodeStackInner {
    fn new(root: NodeEntity) -> Self {
        let root_key = key_from_entity(&root);
        let mut inner = Self {
            graph: StableDiGraph::default(),
            key_to_index: HashMap::new(),
            pending_requirements: HashMap::new(),
            root_key,
        };
        // Root node has no dependencies, so this should never fail.
        inner
            .insert_entity(root, true)
            .expect("root node should have no dependencies");
        inner
    }

    fn insert_entity(&mut self, entity: NodeEntity, validate: bool) -> Result<()> {
        if validate {
            self.validate_dependencies(&entity)?;
        }

        let key = key_from_entity(&entity);
        let index = if let Some(&existing_index) = self.key_to_index.get(&key) {
            // Existing entity: replace the inner value while keeping the same Arc.
            // (No external Arc handles can exist before insert is called for the
            // first time, but for an existing entity replacement we want to keep
            // any external handles valid.)
            if let Some(handle) = self.graph.node_weight(existing_index) {
                let mut guard = handle.write();
                *guard = entity;
            }
            existing_index
        } else {
            let handle: EntityHandle = Arc::new(RwLock::new(entity));
            let idx = self.graph.add_node(handle);
            self.key_to_index.insert(key.clone(), idx);
            idx
        };

        self.rewire_dependencies(index);
        self.resolve_pending_requirements(&key);
        Ok(())
    }

    fn validate_dependencies(&self, entity: &NodeEntity) -> Result<()> {
        let errors = validate_dependency_specs(
            &entity.config().manifest,
            &entity.config().interfaces,
            entity.config().manifest.name.as_str(),
            &entity.config().manifest.tag,
            |name, tag| {
                let key = NodeKey::new(name, tag);
                self.key_to_index
                    .get(&key)
                    .and_then(|&idx| self.graph.node_weight(idx))
                    .map(|handle| handle.read().config().clone())
            },
        );

        if let Some(err) = errors.into_iter().next() {
            return Err(err.into());
        }

        self.validate_no_service_action_cycle(entity)?;

        Ok(())
    }

    /// Reject a service/action dependency cycle that the candidate would close.
    ///
    /// Interface deps are absent from the node-dep graph, so a caller-driven
    /// cycle routed through interfaces is invisible to the structural check.
    /// This runs over the whole stack plus the candidate (using the candidate's
    /// incoming config on a re-add, not the stale stored one) so a cycle
    /// completed across separate invocations is caught the moment the second
    /// node is added.
    fn validate_no_service_action_cycle(&self, entity: &NodeEntity) -> Result<()> {
        let candidate_key = key_from_entity(entity);

        // On a re-add the candidate already lives in the graph, and
        // `push_config_impl` calls us while holding that entity's *write* lock
        // (it validates and replaces the entity under one held lock). Reading
        // its handle here would re-enter a lock the current thread already
        // holds — `parking_lot::RwLock` is not reentrant, so it would
        // self-deadlock. Identify the candidate's slot by index up front and
        // skip it without ever locking it; its incoming config is appended
        // below instead of the stale stored one.
        let candidate_index = self.key_to_index.get(&candidate_key).copied();

        let mut configs: Vec<(String, String, NodeConfig)> =
            Vec::with_capacity(self.graph.node_count() + 1);
        for index in self.graph.node_indices() {
            if Some(index) == candidate_index {
                continue;
            }
            let Some(handle) = self.graph.node_weight(index) else {
                continue;
            };
            let guard = handle.read();
            let key = key_from_entity(&guard);
            configs.push((key.name, key.tag, guard.config().clone()));
        }
        configs.push((
            candidate_key.name,
            candidate_key.tag,
            entity.config().clone(),
        ));

        let view: Vec<CycleCheckNode<'_>> = configs
            .iter()
            .map(|(name, tag, config)| CycleCheckNode { name, tag, config })
            .collect();

        if let Some(cycle) = find_service_action_cycle(&view) {
            return Err(Error::ServiceActionInterfaceCycle {
                nodes: cycle.nodes,
                closing_dependency: cycle.closing_dependency,
                kind: cycle.kind.to_string(),
            });
        }

        Ok(())
    }

    fn rewire_dependencies(&mut self, index: NodeIndex) {
        let existing_edges: Vec<_> = self
            .graph
            .edges_directed(index, Direction::Outgoing)
            .map(|edge| edge.id())
            .collect();
        for edge in existing_edges {
            self.graph.remove_edge(edge);
        }
        self.attach_dependencies(index);
    }

    fn attach_dependencies(&mut self, index: NodeIndex) {
        let keys = if let Some(handle) = self.graph.node_weight(index) {
            dependency_keys(handle.read().config())
        } else {
            return;
        };
        self.clear_pending_requirements_for(index);
        for dep_key in keys {
            if !self.try_attach_edge(index, &dep_key) {
                self.register_pending_requirement(dep_key, index);
            }
        }
    }

    fn clear_pending_requirements_for(&mut self, dependant: NodeIndex) {
        self.pending_requirements.retain(|_, pending| {
            pending.retain(|&idx| idx != dependant);
            !pending.is_empty()
        });
    }

    fn register_pending_requirement(&mut self, dep_key: NodeKey, dependant: NodeIndex) {
        let entry = self.pending_requirements.entry(dep_key).or_default();
        if !entry.contains(&dependant) {
            entry.push(dependant);
        }
    }

    fn try_attach_edge(&mut self, dependant_index: NodeIndex, dep_key: &NodeKey) -> bool {
        let Some(&dependency_index) = self.key_to_index.get(dep_key) else {
            return false;
        };

        if self
            .graph
            .find_edge(dependant_index, dependency_index)
            .is_none()
        {
            self.graph.add_edge(dependant_index, dependency_index, ());
        }
        true
    }

    fn resolve_pending_requirements(&mut self, key: &NodeKey) {
        if !self.key_to_index.contains_key(key) {
            return;
        }

        let Some(pending) = self.pending_requirements.remove(key) else {
            return;
        };

        let mut remaining = Vec::new();
        for dependant_index in pending {
            if !self.try_attach_edge(dependant_index, key) {
                remaining.push(dependant_index);
            }
        }

        if !remaining.is_empty() {
            self.pending_requirements.insert(key.clone(), remaining);
        }
    }

    fn len(&self) -> usize {
        self.graph.node_count()
    }

    fn contains(&self, key: &NodeKey) -> bool {
        self.key_to_index.contains_key(key)
    }

    fn find(&self, key: &NodeKey) -> Option<EntityHandle> {
        self.key_to_index
            .get(key)
            .and_then(|index| self.graph.node_weight(*index))
            .cloned()
    }

    /// Shared entity scan for the instance-lookup methods. Visits every entity
    /// in graph order, holding each entity's read guard while `project`
    /// inspects it, and returns the first `Some` projection. When `skip_root`
    /// is set the synthetic root entity is not visited. Holding the guard
    /// across the projection keeps each lookup atomic with respect to a
    /// concurrent `prepare_and_spawn` / `stop_instance` mutating that entity's
    /// instances list.
    fn find_map_entity<T>(
        &self,
        skip_root: bool,
        mut project: impl FnMut(&EntityHandle, &NodeEntity) -> Option<T>,
    ) -> Option<T> {
        self.graph.node_weights().find_map(|handle| {
            let guard = handle.read();
            if skip_root && self.is_root(&key_from_entity(&guard)) {
                return None;
            }
            project(handle, &guard)
        })
    }

    /// Looks up a tracked instance by id across all entities. **Only matches
    /// `Running` instances** — `Starting` instances are skipped because they
    /// haven't subscribed to messenger services yet, so a handle to one would
    /// let callers reach into something that can't respond. Callers wanting
    /// to clean up an in-flight start should use `NodeEntity::abort_started`
    /// instead.
    fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        self.find_map_entity(false, |_, entity| {
            entity
                .instances()
                .iter()
                .find(|inst| instance_matches(inst, instance_id, true))
                .cloned()
        })
    }

    /// Find the `(node_name, node_tag)` of any entity in the stack that
    /// already tracks an instance with `instance_id`, in any state —
    /// `Starting`, `Running`, etc. Used by the daemon's stack-wide
    /// instance_id uniqueness guard at spawn time (the validator's
    /// `rule 7` is the primary check at plan time; this is the
    /// defensive backstop at the trust boundary).
    ///
    /// Skips the root entity — the daemon's own internals own an
    /// `instance_id`, but it's not user-namable, so a collision there
    /// is structurally impossible.
    fn find_entity_label_for_instance_id_any_state(
        &self,
        instance_id: &Name,
    ) -> Option<(String, String)> {
        self.find_map_entity(true, |_, entity| {
            entity
                .instances()
                .iter()
                .any(|inst| instance_matches(inst, instance_id, false))
                .then(|| {
                    (
                        entity.config().manifest.name.as_str().to_owned(),
                        entity.config().manifest.tag.clone(),
                    )
                })
        })
    }

    /// Same filtering rule as [`find_by_instance_id`]: only entities
    /// containing a `Running` instance with the given id are returned.
    fn find_entity_by_instance_id(&self, instance_id: &Name) -> Option<EntityHandle> {
        self.find_map_entity(false, |handle, entity| {
            entity
                .instances()
                .iter()
                .any(|inst| instance_matches(inst, instance_id, true))
                .then(|| handle.clone())
        })
    }

    fn root(&self) -> EntityHandle {
        self.find(&self.root_key)
            .expect("root node must always exist in NodeStack")
    }

    fn is_root(&self, key: &NodeKey) -> bool {
        &self.root_key == key
    }

    fn entities_snapshot(&self) -> Vec<EntityHandle> {
        self.graph.node_weights().cloned().collect()
    }

    /// Adds a config to the stack or updates an existing one. The resulting
    /// entity is always left in [`NodeStage::Added`] with `config_path` as the
    /// recorded path; the caller must drive [`NodeEntity::build`] to advance
    /// the entity to `Built` before any instances can be started.
    ///
    /// When the entity already exists, this resets its lifecycle:
    /// - the stored config is replaced,
    /// - the stage is rolled back to `Added` with the supplied `config_path`,
    /// - any previous `artifact_path` and instance tracking are dropped.
    ///
    /// Callers must therefore stop / remove any pre-existing instances of the
    /// entity *before* calling `push_config` for a re-add. (See
    /// `shutdown_existing_instances` in `services/node/add.rs`.) The on-disk
    /// `.sif`/archive of the previous build is the caller's responsibility to
    /// clean up — `push_config` only manages the in-memory entity.
    ///
    /// Dependency checks and rewiring only occur when interfaces change.
    /// Returns `Err(CannotModifyRootNode)` if trying to modify the root node config.
    /// Returns `Err(CannotOverwriteNodeWithDependents)` if interfaces change and the node has dependents.
    fn push_config_impl<P: Into<PathBuf>>(
        &mut self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        config_path: P,
    ) -> Result<()> {
        let key = NodeKey::new(config.manifest.name.as_str(), &config.manifest.tag);
        let config_path = config_path.into();

        // The root node cannot be modified
        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        if let Some(&index) = self.key_to_index.get(&key) {
            // We must perform the "no live instances" check, the
            // interfaces/dependency drift checks, AND the wholesale entity
            // replacement under a *single* held write lock on the entity.
            // Dropping the lock between the check and the replacement would
            // let an in-flight `prepare_and_spawn` (which only holds an
            // `Arc<RwLock<NodeEntity>>` and not the stack lock) append a
            // `Starting` instance that the replacement would silently orphan.
            let Some(handle) = self.graph.node_weight(index).cloned() else {
                return Ok(());
            };

            let (interfaces_changed, dependencies_changed) = {
                let mut guard = handle.write();

                if !guard.instances().is_empty() {
                    return Err(Error::CannotOverwriteNodeWithLiveInstances {
                        node_name: key.name.clone(),
                        node_tag: key.tag.clone(),
                    });
                }

                let interfaces_changed = guard.config().interfaces != config.interfaces;
                let old_dependency_keys = dependency_keys(guard.config());
                let new_dependency_keys = dependency_keys(&config);
                let dependencies_changed = old_dependency_keys != new_dependency_keys;

                // Interface changes can break dependents that consume this
                // node, so they need an explicit gate. Dependency-spec
                // changes (e.g. swapping `link_id` of a consumed
                // topic) only affect *this* node's outbound edges, so they
                // don't need the dependents check.
                if interfaces_changed {
                    let has_dependents = self
                        .graph
                        .neighbors_directed(index, Direction::Incoming)
                        .next()
                        .is_some()
                        || self
                            .pending_requirements
                            .get(&key)
                            .is_some_and(|requirements| !requirements.is_empty());

                    if has_dependents {
                        return Err(Error::CannotOverwriteNodeWithDependents {
                            node_name: key.name,
                            node_tag: key.tag,
                        });
                    }
                }

                if interfaces_changed || dependencies_changed {
                    let candidate = NodeEntity::new(config.clone(), config_path.clone());
                    if allow_missing_dependencies {
                        // A permissive (missing-dependency) re-add skips the full
                        // dependency check, but must still not close a
                        // service/action cycle — run the cycle check on the
                        // candidate's incoming config regardless.
                        self.validate_no_service_action_cycle(&candidate)?;
                    } else {
                        self.validate_dependencies(&candidate)?;
                    }
                }

                // Replace the entity in-place under the still-held write
                // lock. The same `Arc` handle is preserved so any external
                // readers see the new state.
                *guard = NodeEntity::new(config, config_path);

                (interfaces_changed, dependencies_changed)
            };

            if interfaces_changed || dependencies_changed {
                self.rewire_dependencies(index);
            }

            Ok(())
        } else {
            // Entity doesn't exist, create new one in the Added stage.
            let entity = NodeEntity::new(config, config_path);
            if allow_missing_dependencies {
                // `insert_entity` skips dependency validation — and with it the
                // cycle check — on a permissive add, so run the cycle check
                // explicitly. The entity is not in the graph yet, so this sees
                // it as the candidate appended to the live stack.
                self.validate_no_service_action_cycle(&entity)?;
            }
            self.insert_entity(entity, !allow_missing_dependencies)?;
            Ok(())
        }
    }

    /// Removes an entity entirely from the graph.
    fn remove_entity(&mut self, key: &NodeKey) {
        if let Some(index) = self.key_to_index.remove(key) {
            self.graph.remove_node(index);
            self.clear_pending_requirements_for(index);
        }
    }

    /// Resolves the graph slot for `key` and confirms it still holds the exact
    /// `Arc<RwLock<NodeEntity>>` the caller captured. Returns the slot's
    /// `NodeIndex` and a clone of the handle when the slot exists, is not the
    /// root, and is pointer-identical to `expected_handle`; returns `None`
    /// otherwise.
    ///
    /// This is the shared identity prologue for the `*_if_matches` cleanup
    /// methods on `NodeStack`. It deliberately stops at the pointer check: each
    /// caller applies its own generation + `Building` stage check under the
    /// specific lock it needs for its mutation, so those checks stay in the
    /// callers rather than being folded in here.
    fn resolve_matching_slot(
        &self,
        key: &NodeKey,
        expected_handle: &Arc<RwLock<NodeEntity>>,
    ) -> Option<(NodeIndex, EntityHandle)> {
        if self.is_root(key) {
            return None;
        }

        let &index = self.key_to_index.get(key)?;
        let current = self.graph.node_weight(index)?.clone();

        if !Arc::ptr_eq(&current, expected_handle) {
            return None;
        }

        Some((index, current))
    }

    /// Clears all nodes except the root node from the stack.
    fn clear(&mut self) {
        let root_handle = self.root();

        self.graph.clear();
        self.key_to_index.clear();
        self.pending_requirements.clear();

        let idx = self.graph.add_node(root_handle);
        self.key_to_index.insert(self.root_key.clone(), idx);
    }

    /// Returns the graph in DOT format for visualization.
    fn to_dot(&self) -> String {
        let dot = Dot::with_attr_getters(
            &self.graph,
            &[Config::EdgeNoLabel, Config::NodeNoLabel],
            &|_, _| String::new(),
            &|_, (_, handle)| {
                let guard = handle.read();
                let name = guard.config().manifest.name.as_str();
                let tag = &guard.config().manifest.tag;
                let stage = guard.stage().name();
                let instance_count = guard.instances().len();
                format!(
                    "label=\"{}:{}\\n[{}] ({} instance{})\"",
                    name,
                    tag,
                    stage,
                    instance_count,
                    if instance_count == 1 { "" } else { "s" }
                )
            },
        );
        format!("{:?}", dot)
    }

    /// Returns a serializable representation of the graph.
    fn to_serialized_graph(&self) -> SerializedNodeGraph {
        // The node list and the edge endpoints must serialize each entity
        // identically. Both go through this closure so the two views cannot
        // drift apart.
        let serialize_entity = |entity: &NodeEntity| SerializedNode::from(entity);

        // One read-lock per entity yields both its serialized node and a clone
        // of its config; the configs feed the interface-conformance edges
        // below, so collecting them here avoids a second locking pass.
        let (nodes, configs): (Vec<SerializedNode>, Vec<NodeConfig>) = self
            .graph
            .node_weights()
            .map(|handle| {
                let guard = handle.read();
                (serialize_entity(&guard), guard.config().clone())
            })
            .unzip();

        // Direct `depends_on.nodes` edges, taken straight from the DAG.
        let mut edges: Vec<SerializedEdge> = self
            .graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (src_idx, dst_idx) = self.graph.edge_endpoints(edge_idx)?;
                let src_handle = self.graph.node_weight(src_idx)?;
                let dst_handle = self.graph.node_weight(dst_idx)?;
                Some(SerializedEdge {
                    from: serialize_entity(&src_handle.read()),
                    to: serialize_entity(&dst_handle.read()),
                    via_interface: None,
                })
            })
            .collect();

        // Interface-conformance edges (`depends_on.interfaces` → a `conforms_to`
        // provider) are deliberately kept out of the DAG so they never constrain
        // launch ordering, but they are real dependencies — surface them in the
        // display graph, annotated with the interface they route through.
        let config_refs: Vec<&NodeConfig> = configs.iter().collect();
        let node_by_key: HashMap<(&str, &str), &SerializedNode> = nodes
            .iter()
            .map(|n| ((n.name.as_str(), n.tag.as_str()), n))
            .collect();
        for edge in collect_interface_conformance_edges(&config_refs) {
            let (Some(from), Some(to)) = (
                node_by_key.get(&(edge.consumer_name.as_str(), edge.consumer_tag.as_str())),
                node_by_key.get(&(edge.provider_name.as_str(), edge.provider_tag.as_str())),
            ) else {
                continue;
            };
            edges.push(SerializedEdge {
                from: (*from).clone(),
                to: (*to).clone(),
                via_interface: Some(format!("{}:{}", edge.interface_name, edge.interface_tag)),
            });
        }

        SerializedNodeGraph { nodes, edges }
    }
}

// ---------------------------------------------------------------------------
// Public thread-safe wrapper
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<NodeStackInner>>,
    /// Most-recent add log path per `(node_name, node_tag)`. This is a
    /// daemon-only cache (not persisted) so it lives here rather than on
    /// `NodeEntity`, which is a pure lifecycle/config model.
    add_log_paths: Arc<parking_lot::Mutex<HashMap<(String, String), PathBuf>>>,
    /// How long a clean daemon shutdown and `peppy node stop` wait for a node to
    /// exit cooperatively before force-killing its process group. Daemon-only
    /// state resolved once from `peppy_config.json5`
    /// (`lifecycle.shutdown_grace_secs`); like `add_log_paths` it lives here, the
    /// shared context every stop path already holds, rather than threading a
    /// `Duration` through each one. Defaults to
    /// `config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS` for constructors
    /// (mostly tests) that don't set it explicitly.
    shutdown_grace: Duration,
}

impl NodeStack {
    /// Creates a new NodeStack with the given root node configuration.
    /// The root node (core node) is the parent of all other nodes in the graph
    /// and cannot be removed from the stack.
    ///
    /// If `instance_id` is `None`, a random instance ID will be generated for
    /// the root node. The root entity's lifecycle is degenerate: it represents
    /// the running daemon itself and has no buildable artifact, so it is
    /// constructed directly in `Ready { instances: [Running] }` via
    /// [`NodeEntity::root`]. The root instance is in `Running` state because
    /// the daemon process is already alive — there's no spawn-then-commit to
    /// model.
    pub fn new<P: Into<PathBuf>>(
        root_config: NodeConfig,
        instance_id: Option<Name>,
        root_path: P,
    ) -> Self {
        let root_path = root_path.into();
        let instance_id = instance_id.unwrap_or_else(|| {
            Name::new(get_random(rng())).expect("random name generation failed")
        });
        let instance = TrackedNodeInstance::new(
            instance_id,
            Some(std::process::id()),
            InstanceState::Running,
            std::collections::BTreeMap::new(),
        );
        let root_entity = NodeEntity::root(root_config, root_path, instance);
        Self {
            shared: Arc::new(RwLock::new(NodeStackInner::new(root_entity))),
            add_log_paths: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            shutdown_grace: Duration::from_secs(config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS),
        }
    }

    /// Sets the cooperative-shutdown grace period (from
    /// `peppy_config.lifecycle.shutdown_grace_secs`). Builder form so the daemon
    /// can configure it at construction without changing [`NodeStack::new`]'s
    /// signature for the many (mostly test) call sites that take the default.
    pub fn with_shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// The cooperative-shutdown grace period a clean daemon shutdown and
    /// `peppy node stop` wait before force-killing a node's process group.
    pub fn shutdown_grace(&self) -> Duration {
        self.shutdown_grace
    }

    /// Records the add-log path for a `(name, tag)` key. Called by the
    /// add/launch handlers after creating the log file.
    pub fn set_add_log_path(&self, name: &str, tag: &str, path: PathBuf) {
        self.add_log_paths
            .lock()
            .insert(add_log_key(name, tag), path);
    }

    /// Returns the most-recent add-log path for `(name, tag)`, if any.
    pub fn add_log_path(&self, name: &str, tag: &str) -> Option<PathBuf> {
        self.add_log_paths
            .lock()
            .get(&add_log_key(name, tag))
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.shared.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the root node (core node) of this stack.
    /// The root node is guaranteed to always exist.
    pub fn root(&self) -> EntityHandle {
        let guard = self.shared.read();
        guard.root()
    }

    pub fn contains(&self, name: &str, tag: &str) -> bool {
        let guard = self.shared.read();
        guard.contains(&NodeKey::new(name, tag))
    }

    /// Returns a shared handle to the entity with the given name and tag, if
    /// any. Callers can read or write through the returned `Arc<RwLock<...>>`
    /// to inspect the entity or to drive lifecycle transitions
    /// (`build` / `start_instance` / `stop_instance`).
    pub fn find(&self, name: &str, tag: &str) -> Option<EntityHandle> {
        let guard = self.shared.read();
        guard.find(&NodeKey::new(name, tag))
    }

    /// Finds a node instance by its instance_id across all entities in the stack.
    pub fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        let guard = self.shared.read();
        guard.find_by_instance_id(instance_id)
    }

    /// Finds a node entity by an instance_id it contains.
    pub fn find_entity_by_instance_id(&self, instance_id: &Name) -> Option<EntityHandle> {
        let guard = self.shared.read();
        guard.find_entity_by_instance_id(instance_id)
    }

    /// Return the `(node_name, node_tag)` of any entity in the stack that
    /// tracks an instance with `instance_id` in any state — `Starting`,
    /// `Running`, etc. Used by the daemon to enforce stack-wide
    /// `instance_id` uniqueness at the spawn trust boundary (per spec
    /// rule 7). The validator catches collisions at plan time; this is
    /// the defensive backstop.
    pub fn find_entity_label_for_instance_id_any_state(
        &self,
        instance_id: &Name,
    ) -> Option<(String, String)> {
        let guard = self.shared.read();
        guard.find_entity_label_for_instance_id_any_state(instance_id)
    }

    /// Adds a config to the stack or updates an existing one.
    ///
    /// New entities are inserted in [`NodeStage::Added`]; the caller is
    /// responsible for transitioning them to `Built` via
    /// [`NodeEntity::build`] before starting any instances.
    ///
    /// If `allow_missing_dependencies` is true, missing dependencies are
    /// tracked as pending requirements and will be wired once the dependency
    /// nodes are added to the stack.
    pub fn push_config<P: Into<PathBuf>>(
        &self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        config_path: P,
    ) -> Result<()> {
        let mut guard = self.shared.write();
        guard.push_config_impl(config, allow_missing_dependencies, config_path)
    }

    pub fn snapshot(&self) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.entities_snapshot()
    }

    /// Removes a node configuration if it has no instances.
    ///
    /// Returns Ok(true) if the config was found and removed, Ok(false) if not found.
    /// Returns Err(CannotModifyRootNode) if trying to remove the root node.
    /// Returns Err(CannotRemoveNodeWithInstances) if the node still has instances.
    pub fn remove_config(&self, name: &str, tag: &str) -> Result<bool> {
        let mut guard = self.shared.write();
        let key = NodeKey::new(name, tag);

        if guard.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        let Some(&index) = guard.key_to_index.get(&key) else {
            return Ok(false);
        };

        // Hold the entity write lock across both the emptiness check and the
        // graph removal so a concurrent `prepare_and_spawn` (which acquires
        // the entity write lock to insert a `Starting` instance) cannot slip
        // an instance in between the check and the unlink. Cloning the
        // handle out of the graph drops the borrow on `guard` immediately so
        // we can still call `guard.remove_entity` further down.
        let Some(entity_handle) = guard.graph.node_weight(index).cloned() else {
            return Ok(false);
        };
        let entity_guard = entity_handle.write();
        if !entity_guard.instances().is_empty() {
            return Err(Error::CannotRemoveNodeWithInstances {
                node_name: name.to_string(),
                node_tag: tag.to_string(),
            });
        }
        guard.remove_entity(&key);
        self.add_log_paths.lock().remove(&add_log_key(name, tag));
        // Keep `entity_guard` alive until *after* the unlink so an outside
        // thread holding a clone of the handle still cannot mutate the
        // entity between the check and the removal.
        drop(entity_guard);
        Ok(true)
    }

    /// Removes the config at `(name, tag)` only if its current entity is in
    /// `Building` AND the underlying `Arc<RwLock<NodeEntity>>` is the same
    /// handle the caller already holds and the entity's `generation` matches.
    ///
    /// Used by the `process_node_add` failure path: after a build error, the
    /// caller wants to remove the entity it created — but a concurrent
    /// `push_config` may have replaced the entity in-place between the
    /// failure and the cleanup. The pointer + generation check rules out the
    /// race: if either differs, the entity is no longer the one we built and
    /// we leave the new state untouched.
    ///
    /// Returns `true` if the entity was removed, `false` otherwise.
    pub fn remove_config_if_matches(
        &self,
        name: &str,
        tag: &str,
        expected_handle: &Arc<RwLock<NodeEntity>>,
        expected_generation: u64,
    ) -> bool {
        let mut guard = self.shared.write();
        let key = NodeKey::new(name, tag);

        let Some((_index, current)) = guard.resolve_matching_slot(&key, expected_handle) else {
            return false;
        };

        let generation_and_stage_match = {
            let entity = current.read();
            entity.generation() == expected_generation
                && matches!(entity.stage(), NodeStage::Building { .. })
        };
        if !generation_and_stage_match {
            return false;
        }

        guard.remove_entity(&key);
        self.add_log_paths.lock().remove(&add_log_key(name, tag));
        true
    }

    /// Rolls the config at `(name, tag)` back from `Building` to `Added` and
    /// re-attaches `working_dir`, but only if its current entity is `Building`
    /// AND the underlying `Arc<RwLock<NodeEntity>>` is the same handle the caller
    /// holds and the entity's `generation` matches.
    ///
    /// Used by the `node_build` `--force` cancellation path: rather than
    /// removing the entity (as the failure path does), the superseded build
    /// leaves it buildable again so the forced rebuild can reuse the same staged
    /// working dir (the only surviving copy of the source). The handle +
    /// generation + `Building` check rules out the race where a concurrent
    /// `push_config` replaced the entity in-place: if any differs, the entity is
    /// no longer the one we built and we leave the new state untouched.
    ///
    /// Returns `true` if the rollback was applied, `false` otherwise.
    pub fn rollback_to_added_if_matches(
        &self,
        name: &str,
        tag: &str,
        expected_handle: &Arc<RwLock<NodeEntity>>,
        expected_generation: u64,
        working_dir: Arc<WorkingDirGuard>,
    ) -> bool {
        // Hold the stack write lock (not read) so a concurrent `push_config`
        // cannot swap the graph node between the identity check and the entity
        // mutation, mirroring `remove_config_if_matches`.
        let guard = self.shared.write();
        let key = NodeKey::new(name, tag);

        let Some((_index, current)) = guard.resolve_matching_slot(&key, expected_handle) else {
            return false;
        };

        let mut entity = current.write();
        if entity.generation() != expected_generation
            || !matches!(entity.stage(), NodeStage::Building { .. })
        {
            return false;
        }
        entity.rollback_building_to_added(working_dir);
        true
    }

    /// Clears all nodes except the root node from the stack.
    pub fn reset(&self) {
        let mut guard = self.shared.write();
        guard.clear();
        self.add_log_paths.lock().clear();
    }

    /// Returns the graph in DOT format for visualization.
    pub fn to_dot(&self) -> String {
        let guard = self.shared.read();
        guard.to_dot()
    }

    /// Returns a serializable representation of the graph.
    pub fn to_serialized_graph(&self) -> SerializedNodeGraph {
        let guard = self.shared.read();
        guard.to_serialized_graph()
    }
}
