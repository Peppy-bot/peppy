pub mod add_steps;
mod build_steps;
mod entity;
mod run_steps;
mod validation;

pub use entity::{
    BuildContext, DependencySpec, NodeEntity, NodeStage, OutputSinks, StartContext,
    StartedInstanceCtx, TrackedNodeInstance, WorkingDirGuard,
};
pub use validation::{collect_dependency_specs, validate_dependency_specs};

use crate::error::{Error, Result};
use config::node::{Interfaces, Name, NodeConfig};
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
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

/// Shared handle to a `NodeEntity` stored inside a `NodeStack`. All readers
/// and writers go through the inner `RwLock`; the same `Arc` is held by the
/// graph and by every external caller, so mutations through `find()` are
/// reflected in subsequent stack queries without any take-and-replace dance.
pub type EntityHandle = Arc<RwLock<NodeEntity>>;

fn key_from_entity(entity: &NodeEntity) -> EntityKey {
    EntityKey::new(
        entity.config().manifest.name.as_str(),
        &entity.config().manifest.tag,
        entity.variant_name(),
    )
}

/// The baseline variant name. Every entity in the stack always has a
/// populated variant; bare `name:tag` shorthand resolves to this value.
pub const DEFAULT_VARIANT: &str = "default";

/// Identifies an entity slot in the stack by `(name, tag, variant)`. Variant
/// is always populated; bare `name:tag` shorthand resolves to
/// [`DEFAULT_VARIANT`].
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct EntityKey {
    pub name: String,
    pub tag: String,
    pub variant: String,
}

impl EntityKey {
    pub fn new(name: &str, tag: &str, variant: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            tag: tag.trim().to_owned(),
            variant: variant.trim().to_owned(),
        }
    }

    /// Renders `name:tag` for the default variant, `name:tag@variant`
    /// otherwise. Use for any user-facing text (logs, errors, list output).
    pub fn label(&self) -> String {
        if self.variant == DEFAULT_VARIANT {
            format!("{}:{}", self.name, self.tag)
        } else {
            format!("{}:{}@{}", self.name, self.tag, self.variant)
        }
    }

    /// Drops the variant, yielding the `(name, tag)` coordinate that
    /// dependents and variant-agnostic lookups key on.
    pub fn name_tag(&self) -> NameTagKey {
        NameTagKey {
            name: self.name.clone(),
            tag: self.tag.clone(),
        }
    }
}

impl std::fmt::Display for EntityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Identifies a `(name, tag)` pair regardless of variant. Used by the
/// validator closure, virtual-deptree sync, and any operation that fans out
/// across every variant of a slot.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct NameTagKey {
    pub name: String,
    pub tag: String,
}

impl NameTagKey {
    pub fn new(name: &str, tag: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            tag: tag.trim().to_owned(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

impl std::fmt::Display for NameTagKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Variant-agnostic reference to a depended-upon node, returned by
/// `dependency_keys`. Distinct from [`NameTagKey`] so the launcher path can
/// statically distinguish "manifest dep target" from "lookup coordinate".
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DepRef {
    pub name: String,
    pub tag: String,
}

impl DepRef {
    pub fn new(name: &str, tag: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            tag: tag.trim().to_owned(),
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

impl std::fmt::Display for DepRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Identifies the slot that [`NodeStack::restore_snapshot_if_matches`]
/// should roll back, together with the handle+generation the caller captured
/// before the failed rebuild.
pub struct RestoreTarget<'a> {
    pub name: &'a str,
    pub tag: &'a str,
    pub variant: &'a str,
    pub expected_handle: &'a Arc<RwLock<NodeEntity>>,
    pub expected_generation: u64,
}

/// Previously-captured entity state used by
/// [`NodeStack::restore_snapshot_if_matches`] to rematerialize a `Ready` /
/// `Added` entity in place of a failed rebuild. Instances are intentionally
/// omitted: any prior instances are shut down before the rebuild begins.
pub struct EntitySnapshot {
    pub config: NodeConfig,
    pub config_path: PathBuf,
    pub artifact_path: Option<PathBuf>,
    pub variant_name: String,
}

/// Returns the variant-agnostic dep references declared by a node's manifest.
/// The launch graph fans each [`DepRef`] out to every running variant of the
/// matched `(name, tag)`.
fn dependency_keys(node: &NodeConfig) -> Vec<DepRef> {
    collect_dependency_specs(node)
        .into_iter()
        .map(|spec| DepRef::new(&spec.node_name, &spec.node_tag))
        .collect()
}

struct NodeStackInner {
    graph: StableDiGraph<EntityHandle, ()>,
    /// Primary index: every entity has a unique [`EntityKey`].
    key_to_index: HashMap<EntityKey, NodeIndex>,
    /// Secondary index: `(name, tag)` to every variant currently in the
    /// graph. Maintained alongside `key_to_index`. Powers variant-agnostic
    /// lookups (dep fan-out, validator closure, "any variant" queries).
    variants_by_name_tag: HashMap<NameTagKey, Vec<NodeIndex>>,
    /// Entities waiting for at least one variant of a `(name, tag)` to
    /// appear. Once any variant arrives, the waiting entities are wired up
    /// and removed from pending. Subsequent variant additions fan out via
    /// `variants_by_name_tag` directly.
    pending_requirements: HashMap<NameTagKey, Vec<NodeIndex>>,
    root_key: EntityKey,
}

impl NodeStackInner {
    fn new(root: NodeEntity) -> Self {
        let root_key = key_from_entity(&root);
        let mut inner = Self {
            graph: StableDiGraph::default(),
            key_to_index: HashMap::new(),
            variants_by_name_tag: HashMap::new(),
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
        let name_tag = key.name_tag();
        let (index, is_new_slot) = if let Some(&existing_index) = self.key_to_index.get(&key) {
            // Existing entity (same name+tag+variant): replace the inner
            // value while keeping the same Arc so any external handles
            // stay valid.
            if let Some(handle) = self.graph.node_weight(existing_index) {
                let mut guard = handle.write();
                *guard = entity;
            }
            (existing_index, false)
        } else {
            let handle: EntityHandle = Arc::new(RwLock::new(entity));
            let idx = self.graph.add_node(handle);
            self.key_to_index.insert(key.clone(), idx);
            self.variants_by_name_tag
                .entry(name_tag.clone())
                .or_default()
                .push(idx);
            (idx, true)
        };

        self.rewire_dependencies(index);
        self.resolve_pending_requirements(&name_tag);
        if is_new_slot {
            self.fan_out_to_existing_dependents(index, &name_tag);
        }
        Ok(())
    }

    fn validate_dependencies(&self, entity: &NodeEntity) -> Result<()> {
        let errors = validate_dependency_specs(
            &entity.config().manifest,
            &entity.config().interfaces,
            entity.config().manifest.name.as_str(),
            &entity.config().manifest.tag,
            |name, tag| {
                // Returns ANY one variant's config because the
                // interface-consistency assertion in `push_config_impl`
                // guarantees every variant of `(name, tag)` exposes
                // identical interfaces. Callers must keep that invariant.
                let nt = NameTagKey::new(name, tag);
                self.variants_by_name_tag
                    .get(&nt)
                    .and_then(|indices| indices.first().copied())
                    .and_then(|idx| self.graph.node_weight(idx))
                    .map(|handle| handle.read().config().clone())
            },
        );

        if let Some(err) = errors.into_iter().next() {
            return Err(err);
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
        let dep_refs = if let Some(handle) = self.graph.node_weight(index) {
            dependency_keys(handle.read().config())
        } else {
            return;
        };
        self.clear_pending_requirements_for(index);
        for dep_ref in dep_refs {
            let dep_nt = NameTagKey::new(&dep_ref.name, &dep_ref.tag);
            let attached = self.try_attach_edges_to_all_variants(index, &dep_nt);
            if !attached {
                self.register_pending_requirement(dep_nt, index);
            }
        }
    }

    fn clear_pending_requirements_for(&mut self, dependant: NodeIndex) {
        self.pending_requirements.retain(|_, pending| {
            pending.retain(|&idx| idx != dependant);
            !pending.is_empty()
        });
    }

    fn register_pending_requirement(&mut self, dep_nt: NameTagKey, dependant: NodeIndex) {
        let entry = self.pending_requirements.entry(dep_nt).or_default();
        if !entry.contains(&dependant) {
            entry.push(dependant);
        }
    }

    /// Adds edges from `dependant_index` to every variant of `dep_nt` that
    /// already exists in the stack. Returns `true` when at least one edge
    /// was added or already present (i.e. the dep is satisfied), `false`
    /// when no variant exists yet.
    fn try_attach_edges_to_all_variants(
        &mut self,
        dependant_index: NodeIndex,
        dep_nt: &NameTagKey,
    ) -> bool {
        let Some(indices) = self.variants_by_name_tag.get(dep_nt).cloned() else {
            return false;
        };
        if indices.is_empty() {
            return false;
        }
        for dependency_index in indices {
            if self
                .graph
                .find_edge(dependant_index, dependency_index)
                .is_none()
            {
                self.graph.add_edge(dependant_index, dependency_index, ());
            }
        }
        true
    }

    fn resolve_pending_requirements(&mut self, name_tag: &NameTagKey) {
        if !self.variants_by_name_tag.contains_key(name_tag) {
            return;
        }

        let Some(pending) = self.pending_requirements.remove(name_tag) else {
            return;
        };

        let mut remaining = Vec::new();
        for dependant_index in pending {
            if !self.try_attach_edges_to_all_variants(dependant_index, name_tag) {
                remaining.push(dependant_index);
            }
        }

        if !remaining.is_empty() {
            self.pending_requirements
                .insert(name_tag.clone(), remaining);
        }
    }

    /// When a new variant of `(name, tag)` is added, every existing entity
    /// whose manifest already declared a dep on that `(name, tag)` must also
    /// gain an edge to the new variant. Without this, dependents added
    /// before the second variant would only fan out across the variants
    /// that existed at their insertion time.
    fn fan_out_to_existing_dependents(&mut self, new_index: NodeIndex, new_nt: &NameTagKey) {
        let dependents: Vec<NodeIndex> = self
            .graph
            .node_indices()
            .filter(|&idx| {
                if idx == new_index {
                    return false;
                }
                let Some(handle) = self.graph.node_weight(idx) else {
                    return false;
                };
                dependency_keys(handle.read().config())
                    .iter()
                    .any(|dep| dep.name == new_nt.name && dep.tag == new_nt.tag)
            })
            .collect();
        for dependant in dependents {
            if self.graph.find_edge(dependant, new_index).is_none() {
                self.graph.add_edge(dependant, new_index, ());
            }
        }
    }

    fn len(&self) -> usize {
        self.graph.node_count()
    }

    fn contains(&self, key: &EntityKey) -> bool {
        self.key_to_index.contains_key(key)
    }

    fn variants_of(&self, name_tag: &NameTagKey) -> Vec<EntityHandle> {
        let Some(indices) = self.variants_by_name_tag.get(name_tag) else {
            return Vec::new();
        };
        indices
            .iter()
            .filter_map(|idx| self.graph.node_weight(*idx).cloned())
            .collect()
    }

    fn find(&self, key: &EntityKey) -> Option<EntityHandle> {
        self.key_to_index
            .get(key)
            .and_then(|index| self.graph.node_weight(*index))
            .cloned()
    }

    /// Looks up a tracked instance by id across all entities. **Only matches
    /// `Running` instances** — `Starting` instances are skipped because they
    /// haven't subscribed to messenger services yet, so a handle to one would
    /// let callers reach into something that can't respond. Callers wanting
    /// to clean up an in-flight start should use `NodeEntity::abort_started`
    /// instead.
    fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        for handle in self.graph.node_weights() {
            let guard = handle.read();
            if let Some(found) = guard
                .instances()
                .iter()
                .find(|inst| {
                    inst.instance_id() == instance_id && inst.state() == InstanceState::Running
                })
                .cloned()
            {
                return Some(found);
            }
        }
        None
    }

    /// Same filtering rule as [`find_by_instance_id`]: only entities
    /// containing a `Running` instance with the given id are returned.
    fn find_entity_by_instance_id(&self, instance_id: &Name) -> Option<EntityHandle> {
        for handle in self.graph.node_weights() {
            let has_instance = {
                let guard = handle.read();
                guard.instances().iter().any(|inst| {
                    inst.instance_id() == instance_id && inst.state() == InstanceState::Running
                })
            };
            if has_instance {
                return Some(handle.clone());
            }
        }
        None
    }

    fn root(&self) -> EntityHandle {
        self.find(&self.root_key)
            .expect("root node must always exist in NodeStack")
    }

    fn is_root(&self, key: &EntityKey) -> bool {
        &self.root_key == key
    }

    fn entities_snapshot(&self) -> Vec<EntityHandle> {
        self.graph.node_weights().cloned().collect()
    }

    fn dependencies_of(&self, key: &EntityKey) -> Vec<EntityHandle> {
        self.key_to_index
            .get(key)
            .map(|index| {
                self.graph
                    .neighbors_directed(*index, Direction::Outgoing)
                    .filter_map(|dep_index| self.graph.node_weight(dep_index))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dependents_of(&self, key: &EntityKey) -> Vec<EntityHandle> {
        self.key_to_index
            .get(key)
            .map(|index| {
                self.graph
                    .neighbors_directed(*index, Direction::Incoming)
                    .filter_map(|dep_index| self.graph.node_weight(dep_index))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Variant-agnostic dependency union: returns every dep of every variant
    /// of `(name, tag)`, deduplicated by graph node index.
    fn dependencies_of_any_variant(&self, name_tag: &NameTagKey) -> Vec<EntityHandle> {
        let Some(variant_indices) = self.variants_by_name_tag.get(name_tag) else {
            return Vec::new();
        };
        let mut seen: HashSet<NodeIndex> = HashSet::new();
        let mut result = Vec::new();
        for &index in variant_indices {
            for dep_index in self.graph.neighbors_directed(index, Direction::Outgoing) {
                if seen.insert(dep_index)
                    && let Some(handle) = self.graph.node_weight(dep_index)
                {
                    result.push(handle.clone());
                }
            }
        }
        result
    }

    /// Variant-agnostic dependent union: returns every dependent of every
    /// variant of `(name, tag)`, deduplicated.
    fn dependents_of_any_variant(&self, name_tag: &NameTagKey) -> Vec<EntityHandle> {
        let Some(variant_indices) = self.variants_by_name_tag.get(name_tag) else {
            return Vec::new();
        };
        let mut seen: HashSet<NodeIndex> = HashSet::new();
        let mut result = Vec::new();
        for &index in variant_indices {
            for dep_index in self.graph.neighbors_directed(index, Direction::Incoming) {
                if seen.insert(dep_index)
                    && let Some(handle) = self.graph.node_weight(dep_index)
                {
                    result.push(handle.clone());
                }
            }
        }
        result
    }

    /// Returns the canonical `Interfaces` for `name_tag`, defined as the
    /// interfaces of the first variant currently in the slot. Used by
    /// `push_config_impl`'s consistency check.
    fn canonical_interfaces_for(&self, name_tag: &NameTagKey) -> Option<(String, Interfaces)> {
        let indices = self.variants_by_name_tag.get(name_tag)?;
        let first_index = *indices.first()?;
        let handle = self.graph.node_weight(first_index)?;
        let guard = handle.read();
        Some((
            guard.variant_name().to_owned(),
            guard.config().interfaces.clone(),
        ))
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
        variant_name: String,
    ) -> Result<Option<EntitySnapshot>> {
        let key = EntityKey::new(
            config.manifest.name.as_str(),
            &config.manifest.tag,
            &variant_name,
        );
        let name_tag = key.name_tag();
        let config_path = config_path.into();

        // The root node cannot be modified
        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        // Interface-consistency assertion: every variant of `(name, tag)`
        // must expose structurally identical `Interfaces`. The first variant
        // inserted in a slot defines the canonical shape; subsequent
        // variants must match modulo ordering / documentation noise.
        // The variant being replaced (same `EntityKey`) is itself the
        // canonical, so skip the check in that case.
        if let Some((canonical_variant, canonical)) = self.canonical_interfaces_for(&name_tag)
            && canonical_variant != variant_name
            && !canonical.matches_unordered(&config.interfaces)
        {
            return Err(Error::InterfaceMismatchAcrossVariants {
                node_name: name_tag.name.clone(),
                node_tag: name_tag.tag.clone(),
                canonical_variant,
                new_variant: variant_name,
            });
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
                return Ok(None);
            };

            let (previous_snapshot, interfaces_changed, dependencies_changed) = {
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
                // changes (e.g. swapping `local_node_id` of a consumed
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
                            .get(&name_tag)
                            .is_some_and(|requirements| !requirements.is_empty());

                    if has_dependents {
                        return Err(Error::CannotOverwriteNodeWithDependents {
                            node_name: key.name,
                            node_tag: key.tag,
                        });
                    }
                }

                if (interfaces_changed || dependencies_changed) && !allow_missing_dependencies {
                    let candidate =
                        NodeEntity::new(config.clone(), config_path.clone(), variant_name.clone());
                    self.validate_dependencies(&candidate)?;
                }

                // Capture the entity state we are about to replace, while
                // still holding the write lock. The caller uses this snapshot
                // for rollback if a subsequent build fails. Capturing here
                // (instead of via a separate read lock before push_config)
                // closes a race window where a concurrent push_config could
                // make the pre-captured snapshot stale.
                let previous_snapshot = EntitySnapshot {
                    config: guard.config().clone(),
                    config_path: guard.config_path().to_path_buf(),
                    artifact_path: guard.artifact_path().map(|p| p.to_path_buf()),
                    variant_name: guard.variant_name().to_owned(),
                };

                // Replace the entity in-place under the still-held write
                // lock. The same `Arc` handle is preserved so any external
                // readers see the new state.
                *guard = NodeEntity::new(config, config_path, variant_name);

                (previous_snapshot, interfaces_changed, dependencies_changed)
            };

            if interfaces_changed || dependencies_changed {
                self.rewire_dependencies(index);
            }

            Ok(Some(previous_snapshot))
        } else {
            // Entity doesn't exist, create new one in the Added stage.
            let entity = NodeEntity::new(config, config_path, variant_name);
            self.insert_entity(entity, !allow_missing_dependencies)?;
            Ok(None)
        }
    }

    /// Removes an entity entirely from the graph.
    fn remove_entity(&mut self, key: &EntityKey) {
        let Some(index) = self.key_to_index.remove(key) else {
            return;
        };
        let name_tag = key.name_tag();
        if let Some(indices) = self.variants_by_name_tag.get_mut(&name_tag) {
            indices.retain(|i| *i != index);
            if indices.is_empty() {
                self.variants_by_name_tag.remove(&name_tag);
            }
        }
        self.graph.remove_node(index);
        self.clear_pending_requirements_for(index);
    }

    /// Clears all nodes except the root node from the stack.
    fn clear(&mut self) {
        let root_handle = self.root();

        self.graph.clear();
        self.key_to_index.clear();
        self.variants_by_name_tag.clear();
        self.pending_requirements.clear();

        let idx = self.graph.add_node(root_handle);
        self.key_to_index.insert(self.root_key.clone(), idx);
        self.variants_by_name_tag
            .entry(self.root_key.name_tag())
            .or_default()
            .push(idx);
    }

    /// Returns the graph in DOT format for visualization. Renders
    /// `name:tag` for the default variant and `name:tag@variant` for any
    /// non-default variant so multi-variant graphs disambiguate visually.
    fn to_dot(&self) -> String {
        let dot = Dot::with_attr_getters(
            &self.graph,
            &[Config::EdgeNoLabel, Config::NodeNoLabel],
            &|_, _| String::new(),
            &|_, (_, handle)| {
                let guard = handle.read();
                let key = EntityKey::new(
                    guard.config().manifest.name.as_str(),
                    &guard.config().manifest.tag,
                    guard.variant_name(),
                );
                let stage = guard.stage().name();
                let instance_count = guard.instances().len();
                format!(
                    "label=\"{}\\n[{}] ({} instance{})\"",
                    key.label(),
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
        let nodes = self
            .graph
            .node_weights()
            .map(|handle| {
                let guard = handle.read();
                SerializedNode::from(&*guard)
            })
            .collect();

        let edges = self
            .graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (src_idx, dst_idx) = self.graph.edge_endpoints(edge_idx)?;
                let src_handle = self.graph.node_weight(src_idx)?;
                let dst_handle = self.graph.node_weight(dst_idx)?;
                let src_guard = src_handle.read();
                let dst_guard = dst_handle.read();
                Some(SerializedEdge {
                    from: SerializedNode::from(&*src_guard),
                    to: SerializedNode::from(&*dst_guard),
                })
            })
            .collect();

        SerializedNodeGraph { nodes, edges }
    }
}

// ---------------------------------------------------------------------------
// Public thread-safe wrapper
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<NodeStackInner>>,
    /// Most-recent add log path per [`EntityKey`]. This is a daemon-only
    /// cache (not persisted) so it lives here rather than on `NodeEntity`,
    /// which is a pure lifecycle/config model.
    add_log_paths: Arc<parking_lot::Mutex<HashMap<EntityKey, PathBuf>>>,
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
        );
        let root_entity = NodeEntity::root(root_config, root_path, instance);
        Self {
            shared: Arc::new(RwLock::new(NodeStackInner::new(root_entity))),
            add_log_paths: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Records the add-log path for the variant slot identified by
    /// `(name, tag, variant)`. Called by the add/launch handlers after
    /// creating the log file.
    pub fn set_add_log_path(&self, name: &str, tag: &str, variant: &str, path: PathBuf) {
        self.add_log_paths
            .lock()
            .insert(EntityKey::new(name, tag, variant), path);
    }

    /// Returns the most-recent add-log path for the variant slot identified
    /// by `(name, tag, variant)`, if any.
    pub fn add_log_path(&self, name: &str, tag: &str, variant: &str) -> Option<PathBuf> {
        self.add_log_paths
            .lock()
            .get(&EntityKey::new(name, tag, variant))
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

    /// Returns `true` if the variant slot `(name, tag, variant)` is in the
    /// stack. Use [`Self::contains_any_variant`] for the variant-agnostic
    /// check.
    pub fn contains(&self, name: &str, tag: &str, variant: &str) -> bool {
        let guard = self.shared.read();
        guard.contains(&EntityKey::new(name, tag, variant))
    }

    /// Returns `true` if at least one variant of `(name, tag)` exists.
    pub fn contains_any_variant(&self, name: &str, tag: &str) -> bool {
        let guard = self.shared.read();
        !guard.variants_of(&NameTagKey::new(name, tag)).is_empty()
    }

    /// Returns a shared handle to the variant slot `(name, tag, variant)`.
    /// Callers can read or write through the returned `Arc<RwLock<...>>` to
    /// inspect the entity or drive lifecycle transitions (`build` /
    /// `start_instance` / `stop_instance`).
    pub fn find(&self, name: &str, tag: &str, variant: &str) -> Option<EntityHandle> {
        let guard = self.shared.read();
        guard.find(&EntityKey::new(name, tag, variant))
    }

    /// Returns every variant of `(name, tag)` currently in the stack. Empty
    /// when the slot has no variants.
    pub fn find_all_variants(&self, name: &str, tag: &str) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.variants_of(&NameTagKey::new(name, tag))
    }

    /// Returns the first available variant of `(name, tag)`, or `None` if
    /// the slot has no variants. Use this for variant-agnostic operations
    /// (validator closures, dependency resolution) where every variant in
    /// the slot is guaranteed to be interface-equivalent (see the
    /// interface-consistency assertion in `push_config_impl`).
    pub fn find_any_variant(&self, name: &str, tag: &str) -> Option<EntityHandle> {
        let guard = self.shared.read();
        guard
            .variants_of(&NameTagKey::new(name, tag))
            .into_iter()
            .next()
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

    /// Adds a config to the stack or updates an existing one. The variant
    /// slot defaults to [`crate::DEFAULT_VARIANT`]; callers that need to
    /// pin a non-default variant must use [`Self::push_config_with_variant`].
    ///
    /// New entities are inserted in [`NodeStage::Added`]; the caller is
    /// responsible for transitioning them to `Built` via
    /// [`NodeEntity::build`] before starting any instances.
    ///
    /// If `allow_missing_dependencies` is true, missing dependencies are
    /// tracked as pending requirements and will be wired once the dependency
    /// nodes are added to the stack.
    ///
    /// Callers that need to roll back to the previous entity state on a
    /// later build failure should use [`Self::push_config_capturing_previous`]
    /// instead so the snapshot is captured atomically with the replacement.
    pub fn push_config<P: Into<PathBuf>>(
        &self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        config_path: P,
    ) -> Result<()> {
        self.push_config_with_variant(
            config,
            allow_missing_dependencies,
            config_path,
            crate::DEFAULT_VARIANT.to_owned(),
        )
    }

    /// Like [`Self::push_config`] but pins a specific variant label. The
    /// variant is stored as first-class state on the resulting
    /// [`NodeEntity`] and exposed via [`NodeEntity::variant_name`]. Passing
    /// [`crate::DEFAULT_VARIANT`] is equivalent to [`Self::push_config`].
    pub fn push_config_with_variant<P: Into<PathBuf>>(
        &self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        config_path: P,
        variant_name: String,
    ) -> Result<()> {
        self.push_config_capturing_previous(
            config,
            allow_missing_dependencies,
            config_path,
            variant_name,
        )
        .map(|_| ())
    }

    /// Like [`Self::push_config_with_variant`] but additionally returns the
    /// snapshot of the entity that was just replaced (if any), captured
    /// under the same write lock that performed the in-place replacement.
    /// The snapshot is suitable for [`Self::restore_snapshot_if_matches`]
    /// rollback after a failed rebuild: capturing it here closes the race
    /// window where a concurrent `push_config` could otherwise make a
    /// pre-captured snapshot stale.
    pub fn push_config_capturing_previous<P: Into<PathBuf>>(
        &self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        config_path: P,
        variant_name: String,
    ) -> Result<Option<EntitySnapshot>> {
        let mut guard = self.shared.write();
        guard.push_config_impl(
            config,
            allow_missing_dependencies,
            config_path,
            variant_name,
        )
    }

    pub fn snapshot(&self) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.entities_snapshot()
    }

    /// Returns the dependencies of the variant slot `(name, tag, variant)`.
    pub fn dependencies_of(&self, name: &str, tag: &str, variant: &str) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.dependencies_of(&EntityKey::new(name, tag, variant))
    }

    /// Returns the union of dependencies across every variant of
    /// `(name, tag)`, deduplicated.
    pub fn dependencies_of_any_variant(&self, name: &str, tag: &str) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.dependencies_of_any_variant(&NameTagKey::new(name, tag))
    }

    /// Returns the dependents of the variant slot `(name, tag, variant)`.
    pub fn dependents_of(&self, name: &str, tag: &str, variant: &str) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.dependents_of(&EntityKey::new(name, tag, variant))
    }

    /// Returns the union of dependents across every variant of
    /// `(name, tag)`, deduplicated. Useful at the daemon layer where
    /// variant-agnostic manifest deps mean a parent depends on every variant
    /// at once.
    pub fn dependents_of_any_variant(&self, name: &str, tag: &str) -> Vec<EntityHandle> {
        let guard = self.shared.read();
        guard.dependents_of_any_variant(&NameTagKey::new(name, tag))
    }

    /// Removes a variant slot if it has no instances.
    ///
    /// Returns `Ok(true)` if the variant was found and removed, `Ok(false)`
    /// if not found. Returns `Err(CannotModifyRootNode)` for the root node.
    /// Returns `Err(CannotRemoveNodeWithInstances)` if the variant still
    /// has instances.
    pub fn remove_config(&self, name: &str, tag: &str, variant: &str) -> Result<bool> {
        let mut guard = self.shared.write();
        let key = EntityKey::new(name, tag, variant);

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
        self.add_log_paths.lock().remove(&key);
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
        variant: &str,
        expected_handle: &Arc<RwLock<NodeEntity>>,
        expected_generation: u64,
    ) -> bool {
        let mut guard = self.shared.write();
        let key = EntityKey::new(name, tag, variant);

        if guard.is_root(&key) {
            return false;
        }

        let Some(&index) = guard.key_to_index.get(&key) else {
            return false;
        };

        let matches = guard.graph.node_weight(index).is_some_and(|current| {
            Arc::ptr_eq(current, expected_handle) && {
                let entity = current.read();
                entity.generation() == expected_generation
                    && matches!(entity.stage(), NodeStage::Building { .. })
            }
        });

        if !matches {
            return false;
        }

        guard.remove_entity(&key);
        self.add_log_paths.lock().remove(&key);
        true
    }

    /// Slot identity for [`NodeStack::restore_snapshot_if_matches`]: the
    /// name/tag key plus the handle+generation the caller captured before
    /// starting the rebuild. Bundled so callers can express the rollback
    /// target as a single value.
    ///
    /// See the free-standing [`RestoreTarget`] and [`EntitySnapshot`] structs
    /// defined below — they are intentionally kept as plain data so the
    /// call-site reads as `stack.restore_snapshot_if_matches(target, snap)`.
    ///
    /// Atomically restore an entity to a previously captured snapshot, iff the
    /// slot still holds the expected handle+generation. Used by the node-add
    /// rebuild path to roll back to the prior `Ready` state when a rebuild
    /// fails after `push_config` has already replaced the entity in-place.
    ///
    /// Returns `true` if the slot matched and was restored. Returns `false`
    /// (without mutating) if the handle was replaced concurrently, the
    /// generation drifted, or the entity is no longer in `Building`.
    pub fn restore_snapshot_if_matches(
        &self,
        target: RestoreTarget<'_>,
        snapshot: EntitySnapshot,
    ) -> bool {
        let guard = self.shared.write();
        let key = EntityKey::new(target.name, target.tag, target.variant);

        if guard.is_root(&key) {
            return false;
        }

        let Some(&index) = guard.key_to_index.get(&key) else {
            return false;
        };

        let Some(current) = guard.graph.node_weight(index) else {
            return false;
        };
        if !Arc::ptr_eq(current, target.expected_handle) {
            return false;
        }
        {
            let entity = current.read();
            if entity.generation() != target.expected_generation
                || !matches!(entity.stage(), NodeStage::Building { .. })
            {
                return false;
            }
        }

        // Rebuild a fresh `Ready`/`Added` entity from the captured snapshot
        // and swap it into place under the same handle. Instances are
        // intentionally empty: any prior instances were shut down before the
        // rebuild began (see `shutdown_existing_instances` in the add path).
        let restored = NodeEntity::from_snapshot(
            snapshot.config,
            snapshot.config_path,
            snapshot.artifact_path,
            Vec::new(),
            snapshot.variant_name,
        );
        *current.write() = restored;
        true
    }

    /// Clears all nodes except the root node from the stack.
    pub fn reset(&self) {
        let mut guard = self.shared.write();
        guard.clear();
        self.add_log_paths.lock().clear();
    }

    /// Applies the state from another NodeStack to this one.
    ///
    /// This resets the current stack (preserving only the root node), then
    /// copies all non-root entities and their built/started state from the
    /// source stack. Because the source artifacts already exist on disk, this
    /// uses [`NodeEntity::from_snapshot`] to materialize each entity directly
    /// in the appropriate stage instead of re-running the I/O-heavy `build()`
    /// step or the `prepare_and_spawn` lifecycle.
    pub fn apply_from(&self, source: &NodeStack) -> std::result::Result<(), String> {
        let target_root = self.root();
        let target_root_guard = target_root.read();
        let target_root_name = target_root_guard.config().manifest.name.as_str().to_owned();
        let target_root_tag = target_root_guard.config().manifest.tag.clone();
        drop(target_root_guard);

        // Phase 1: validate every source entity and pre-build its
        // replacement `NodeEntity` without touching `self`. Dedupes on
        // `EntityKey` so two source variants of the same `(name, tag)`
        // don't silently collapse into one slot at insert time.
        let mut prepared: Vec<(EntityKey, NodeEntity)> = Vec::new();
        let mut seen_keys: HashSet<EntityKey> = HashSet::new();
        for source_handle in source.snapshot() {
            let source_guard = source_handle.read();
            let config = source_guard.config().clone();

            // Skip the root node from the source stack
            if config.manifest.name.as_str() == target_root_name.as_str()
                && config.manifest.tag == target_root_tag
            {
                continue;
            }

            let name = config.manifest.name.as_str().to_owned();
            let tag = config.manifest.tag.clone();
            let config_path = source_guard.config_path().to_path_buf();
            let artifact_path = source_guard.artifact_path().map(|p| p.to_path_buf());
            let instances: Vec<TrackedNodeInstance> = source_guard.instances().to_vec();
            let variant_name = source_guard.variant_name().to_owned();

            // Reject transient lifecycle state from the source: snapshot
            // replay only makes sense for entities that are quiescent (no
            // in-flight build, no in-flight start). A `Building` source has
            // no artifact yet, and `Starting` instances reference live
            // child/reader handles that we cannot recreate from a snapshot.
            match source_guard.stage() {
                NodeStage::Added { .. } => {}
                NodeStage::Ready {
                    instances: src_instances,
                    ..
                } => {
                    if let Some(bad) = src_instances
                        .iter()
                        .find(|i| i.state() != InstanceState::Running)
                    {
                        return Err(format!(
                            "cannot replay live entity {}:{}@{}: instance {} is in transient state {:?}",
                            name,
                            tag,
                            variant_name,
                            bad.instance_id().as_str(),
                            bad.state(),
                        ));
                    }
                }
                NodeStage::Building { .. } => {
                    return Err(format!(
                        "cannot replay live entity {}:{}@{}: source is currently Building",
                        name, tag, variant_name
                    ));
                }
                NodeStage::Root { .. } => {
                    // Should already be skipped by the root-name guard above,
                    // but be defensive: never replay a Root variant.
                    continue;
                }
            }
            drop(source_guard);

            let key = EntityKey::new(&name, &tag, &variant_name);
            if !seen_keys.insert(key.clone()) {
                return Err(format!(
                    "duplicate source entity for {}: snapshot replay requires \
                     each (name, tag, variant) slot to appear at most once",
                    key.label()
                ));
            }

            // Materialize the entity directly in the appropriate stage. The
            // `from_snapshot` constructor bypasses the lifecycle because the
            // source artifact already exists on disk.
            let entity = NodeEntity::from_snapshot(
                config,
                config_path,
                artifact_path,
                instances,
                variant_name,
            );
            prepared.push((key, entity));
        }

        // Phase 2: only after *every* source entity has validated, clear the
        // target and insert the prepared entities under a single held write
        // lock so readers never observe a partially-restored graph (e.g. a
        // cleared stack with only some entities re-inserted).
        let mut guard = self.shared.write();
        guard.clear();
        self.add_log_paths.lock().clear();
        for (key, entity) in prepared {
            guard
                .insert_entity(entity, false)
                .map_err(|e| format!("failed to insert snapshot for {}: {e}", key.label()))?;
        }

        Ok(())
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
