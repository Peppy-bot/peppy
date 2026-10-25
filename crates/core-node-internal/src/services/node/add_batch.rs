//! Pinned batch-add pipeline for `NodeSource::Pinned` and
//! `NodeSource::ResolveRef` goals.
//!
//! A pinned goal arrives with its whole closure decided: the root pin in the
//! source, dependency-node and document pins in `pins_json5`. This executor
//! materializes exactly that set through [`super::pins`], reusing local
//! content when its fingerprint matches and fetching the pinned commit when
//! it does not, then topologically sorts the batch via [`VirtualDeptree`]
//! and feeds each node to [`super::add::run_node_add`]. Nothing here looks a
//! name up: an identity the pins do not cover and the stack does not hold is
//! a refusal, never a lookup.
//!
//! A `ResolveRef` goal is the entry arm for `peppy node add <name>:<tag>`:
//! the receiving daemon resolves the closure against its own caches, minting
//! the same pins a launch coordinator would, and continues down the
//! identical pinned pipeline. Name resolution therefore exists in exactly
//! one place ([`super::pins::resolve_pinned_closure`]) whichever machine
//! runs it.
//!
//! Mid-batch failure rolls back every node pushed during the batch via a
//! drop guard.

use super::super::stack::STACK_LAUNCH_GIT_HASH;
use super::FeedbackLine;
use super::add::{NodeAddActionContext, record_add_failure, run_node_add};
use super::pins::{self, MaterializedPin};
use crate::services::repo::cache as repo_cache;
use crate::services::response::rephrased;
use config::failure_kind::PeppyFailureKind;
use config::node::NodeConfig;
use core_node_api::encoding::{NodeAddGoal, NodeAddResult, NodeSource};
use daemon_config::repository::{PinKind, PinnedItem};
use node_stack::{ActionLog, Announcer, NodeEntity, VirtualDeptree, WorkingDirGuard};
use peppylib::messaging::Failure;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Entry point [`super::add::dispatch_node_add`] routes to when
/// `goal.source` is `NodeSource::Pinned` or `NodeSource::ResolveRef`.
pub(crate) async fn run_pinned_add(
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    announcer: Announcer,
) -> Result<NodeAddResult, Failure> {
    let peppy_dirs = action_context.peppy_dirs.clone();
    let fail = |kind: PeppyFailureKind, message: String| {
        Err(record_add_failure(&announcer, Failure::new(kind, message)))
    };
    let on_feedback: super::cache::MaterializeFeedback =
        Arc::new(super::step_sink(announcer.clone()));

    // An ABSENT cache is not a problem on the pinned path: content this
    // machine does not hold is fetched from each pin's own origin, which is
    // what lets a freshly-provisioned peer join a launch. A cache that exists
    // but does not parse still refuses, because a machine with broken state
    // should say so rather than quietly re-fetching everything on every
    // launch. Loaded once, before the arms, and shared with every
    // materialization rather than copied per pin.
    let entries = match repo_cache::load_node_cache(&peppy_dirs) {
        Ok(loaded) => Arc::new(loaded),
        Err(e) => {
            return fail(
                PeppyFailureKind::AttemptFailed,
                format!("Failed to read nodes cache: {}", e),
            );
        }
    };

    let (nodes, doc_pins_json5) = match &goal.source {
        NodeSource::Pinned { pin_json5 } => {
            let root: daemon_config::repository::PinnedItem = match serde_json5::from_str(pin_json5)
            {
                Ok(pin) => pin,
                Err(e) => {
                    return fail(
                        PeppyFailureKind::Rejected,
                        format!("the goal's root pin is not decodable: {e}"),
                    );
                }
            };
            if root.kind != PinKind::Node {
                return fail(
                    PeppyFailureKind::Rejected,
                    format!("the goal's root pin is {}, not a node", root.label()),
                );
            }
            let closure = match pins::decode_pins(&goal.pins_json5) {
                Ok(closure) => closure,
                Err(e) => return fail(PeppyFailureKind::Rejected, e),
            };
            // Re-encoded from the decoded pins rather than sliced out of the
            // raw input by position: the split is stated over the values that
            // carry the kind, not over an assumed correspondence between two
            // collections, and it is the same round-trip the `ResolveRef` arm
            // performs below.
            let (node_pins, doc_pins): (Vec<_>, Vec<_>) = closure
                .into_iter()
                .partition(|pin| pin.kind == PinKind::Node);
            let doc_pins_json5 = match pins::encode_pins(&doc_pins) {
                Ok(encoded) => encoded,
                Err(e) => return fail(PeppyFailureKind::AttemptFailed, e),
            };

            announcer.line(format!(
                "Materializing {} pinned node(s) for {}",
                node_pins.len() + 1,
                root.label()
            ));

            let nodes = match pins::materialize_pin_set(
                &peppy_dirs,
                &entries,
                root,
                node_pins,
                on_feedback,
            )
            .await
            {
                Ok(nodes) => nodes,
                Err(e) => return fail(PeppyFailureKind::AttemptFailed, e),
            };
            (nodes, doc_pins_json5)
        }
        NodeSource::ResolveRef { name, tag } => {
            announcer.line(format!("Resolving {}:{} from repo cache", name, tag));
            if entries.is_empty() {
                return fail(
                    PeppyFailureKind::PreconditionFailed,
                    format!(
                        "nodes.json5 not found or empty at {}; run `peppy repo refresh` to \
                         populate it",
                        repo_cache::nodes_repo_cache_path(&peppy_dirs).display()
                    ),
                );
            }
            let closure = match pins::resolve_pinned_closure(
                &peppy_dirs,
                &entries,
                name,
                tag,
                Arc::clone(&on_feedback),
            )
            .await
            {
                Ok(closure) => closure,
                Err(failure) => return Err(record_add_failure(&announcer, failure)),
            };
            let minted = pins::doc_pins_for_manifest_sets_async(
                &peppy_dirs,
                vec![closure.manifests()],
                Arc::clone(&on_feedback),
            )
            .await
            .and_then(|mut sets| sets.remove(0));
            let doc_pins = match minted {
                Ok(doc_pins) => doc_pins,
                Err(e) => return fail(PeppyFailureKind::AttemptFailed, e),
            };
            let doc_pins_json5 = match pins::encode_pins(&doc_pins) {
                Ok(encoded) => encoded,
                Err(e) => return fail(PeppyFailureKind::AttemptFailed, e),
            };
            (closure.nodes, doc_pins_json5)
        }
        NodeSource::Fs(_)
        | NodeSource::Git { .. }
        | NodeSource::Http { .. }
        | NodeSource::Exposures { .. } => {
            return fail(
                PeppyFailureKind::AttemptFailed,
                "internal error: run_pinned_add called with a non-pinned source".to_owned(),
            );
        }
    };

    execute_pinned_batch(nodes, doc_pins_json5, goal, action_context, announcer).await
}

/// Adds a materialized pin set to the stack in dependency order.
///
/// The batch is exactly the pin set. A dependency an entry declares that the
/// pins do not cover must already be in the stack (a launch adds
/// deployments in dependency order, so a dependency planned as its own
/// deployment landed earlier); one that is in neither is refused as an
/// incomplete closure rather than looked up by name.
async fn execute_pinned_batch(
    nodes: Vec<MaterializedPin>,
    doc_pins_json5: Vec<String>,
    goal: NodeAddGoal,
    action_context: NodeAddActionContext,
    announcer: Announcer,
) -> Result<NodeAddResult, Failure> {
    let fail = |kind: PeppyFailureKind, message: String| {
        Err(record_add_failure(&announcer, Failure::new(kind, message)))
    };
    let root_label = {
        let root = nodes.first().filter(|node| node.is_root);
        match root {
            Some(root) => (
                root.pin.name.as_str().to_owned(),
                root.pin.tag.as_str().to_owned(),
            ),
            None => {
                return fail(
                    PeppyFailureKind::AttemptFailed,
                    "internal error: a pinned batch arrived without its root".to_owned(),
                );
            }
        }
    };

    let gaps = closure_gaps(&nodes, |name, tag| {
        action_context.node_stack.find(name, tag).is_some()
    });
    if !gaps.is_empty() {
        return fail(
            PeppyFailureKind::Rejected,
            daemon_config::format_bulleted(&gaps),
        );
    }

    let tree_input: Vec<(PathBuf, NodeConfig)> = nodes
        .iter()
        .map(|node| (node.root_dir.clone(), node.config.clone()))
        .collect();
    let tree = match VirtualDeptree::build(tree_input) {
        Ok(t) => t,
        Err(e) => {
            return fail(
                PeppyFailureKind::Rejected,
                format!("Dependency resolution failed: {}", e),
            );
        }
    };

    let node_lookup: HashMap<(String, String), &MaterializedPin> = nodes
        .iter()
        .map(|node| {
            (
                (
                    node.pin.name.as_str().to_owned(),
                    node.pin.tag.as_str().to_owned(),
                ),
                node,
            )
        })
        .collect();

    announcer.line(format!(
        "Batch resolved: {} node(s) to add: {}",
        nodes.len(),
        nodes
            .iter()
            .map(|node| format!("{}:{}", node.pin.name, node.pin.tag))
            .collect::<Vec<_>>()
            .join(", ")
    ));

    let mut rollback = RollbackGuard::new(Arc::clone(&action_context.node_stack));

    for info in tree.topological_order() {
        let key = info.key();
        let Some(node) = node_lookup.get(&key) else {
            return fail(
                PeppyFailureKind::AttemptFailed,
                format!(
                    "internal error: topological order produced a node not in the batch ({}:{})",
                    key.0, key.1
                ),
            );
        };

        // A dependency the stack already holds with this exact manifest is
        // left alone: re-pushing it would reset the entity to `Added` and
        // silently discard a built artifact, which breaks any flow that
        // built the node earlier and still expects to run it (a stack
        // launch adds and builds each deployment in turn, so a dependency
        // shared with an earlier deployment has been built by the time it
        // reappears in a later one's closure). The batch ROOT is exempt:
        // an explicit `node add X` re-stages X, identical or not.
        if !node.is_root && stack_holds_identical(&action_context.node_stack, node) {
            announcer.line(format!(
                "Skipping {}:{}: already in the stack with an identical manifest",
                node.pin.name, node.pin.tag
            ));
            continue;
        }

        announcer.line(format!(
            "Adding {}:{} ({})",
            node.pin.name,
            node.pin.tag,
            node.pin.origin.kind().as_str()
        ));

        // Snapshot any pre-existing config before the sub-add replaces it
        // in place. On rollback we re-install this config instead of
        // removing the slot, so an in-place replacement that later fails
        // elsewhere in the batch does not wipe the user's prior state.
        //
        // We also clone the previous entity's `pending_working_dir` Arc so
        // its `WorkingDirGuard` stays alive through the sub-add. The
        // sub-add's `push_config` drops the previous entity (and its only
        // owning reference to the guard); without this clone the guard
        // would drop and the temp dir backing `config_path` would be
        // removed before rollback ever runs. The pins of the documents its
        // code was generated from go with it, so `instance_documents` still
        // answers for an instance of the restored node.
        let previous = action_context
            .node_stack
            .find(node.pin.name.as_str(), node.pin.tag.as_str())
            .map(|handle| PreviousConfig::of(&handle.read()));

        if let Err(failure) =
            run_single_batched_add(node, &doc_pins_json5, &goal, &action_context, &announcer).await
        {
            let message = format!(
                "Failed to add {}:{}: {}",
                node.pin.name,
                node.pin.tag,
                failure.message()
            );
            return Err(record_add_failure(&announcer, rephrased(&failure, message)));
        }

        rollback.added.push(RollbackEntry {
            name: node.pin.name.as_str().to_owned(),
            tag: node.pin.tag.as_str().to_owned(),
            previous,
        });
    }

    // Batch succeeded; defuse rollback and report. Counted from what rollback
    // recorded rather than from the pin set: a dependency the stack already
    // held with an identical manifest was skipped, and reporting it as added
    // would describe work this batch did not do.
    let added = rollback.added.len();
    rollback.disarm();

    announcer.line(format!("Batch add complete: {added} node(s) added"));

    Ok(NodeAddResult::new(root_label.0, root_label.1))
}

/// Dependencies the batch can neither add nor find: named by a materialized
/// manifest, absent from the pin set, and absent from the stack.
///
/// The batch is exactly the pin set, so this is the boundary where "never
/// resolve a name" bites: a gap is refused as an incomplete closure rather
/// than looked up. A dependency in the stack is not a gap, because a launch
/// adds deployments in dependency order and a dependency planned as its own
/// deployment landed there before this batch ran.
fn closure_gaps(nodes: &[MaterializedPin], in_stack: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let pinned_keys: HashSet<(String, String)> = nodes
        .iter()
        .map(|node| {
            (
                node.pin.name.as_str().to_owned(),
                node.pin.tag.as_str().to_owned(),
            )
        })
        .collect();
    let mut gaps: Vec<String> = Vec::new();
    for node in nodes {
        let Some(deps) = node.config.manifest.depends_on.as_ref() else {
            continue;
        };
        for dep in &deps.nodes {
            let key = (dep.name.as_str().to_owned(), dep.tag.clone());
            if pinned_keys.contains(&key) || in_stack(&key.0, &key.1) {
                continue;
            }
            gaps.push(format!(
                "{} depends on `{}:{}`, which is neither pinned nor in the stack",
                node.pin.label(),
                key.0,
                key.1
            ));
        }
    }
    if !gaps.is_empty() {
        gaps.sort();
        gaps.insert(
            0,
            "this add's pins do not cover its closure; the coordinator that minted them \
             shipped an incomplete set"
                .to_owned(),
        );
    }
    gaps
}

/// Whether the stack already holds `node`'s identity with a config whose
/// canonical fingerprint matches the freshly materialized one.
///
/// Compared by [`super::manifest_fingerprint`] rather than by identity
/// alone: an entry whose pin moved to a different manifest must still be
/// replaced, or the batch would keep satisfying dependents with a config
/// the launch no longer describes. A fingerprint that fails to
/// compute reports "different", falling back to the replace path.
fn stack_holds_identical(node_stack: &node_stack::NodeStack, node: &MaterializedPin) -> bool {
    let Some(handle) = node_stack.find(node.pin.name.as_str(), node.pin.tag.as_str()) else {
        return false;
    };
    let existing = { handle.read().config().clone() };
    match (
        super::manifest_fingerprint(&existing),
        super::manifest_fingerprint(&node.config),
    ) {
        (Ok(existing), Ok(incoming)) => existing == incoming,
        _ => false,
    }
}

/// Run a single node-add step inside the batch. Each sub-add gets its
/// own log file, which the batch's log names, so the user finds the log of
/// each node from the log the goal response names. The batch's doc pins
/// ride on every sub-goal, so each node's contract and pairing documents
/// resolve to the launch's bytes rather than this machine's cache picks.
async fn run_single_batched_add(
    node: &MaterializedPin,
    doc_pins_json5: &[String],
    batch_goal: &NodeAddGoal,
    action_context: &NodeAddActionContext,
    batch_announcer: &Announcer,
) -> Result<NodeAddResult, Failure> {
    let mut sub_goal = NodeAddGoal::new(
        node.root_dir.clone(),
        STACK_LAUNCH_GIT_HASH,
        batch_goal.timeout_secs,
    )
    .with_pins(doc_pins_json5.to_vec());

    if node.is_root {
        // Env vars / force only apply to the root entity.
        sub_goal = sub_goal
            .with_env_vars(batch_goal.env_vars.clone())
            .with_force(batch_goal.force);
    }

    // Each sub-add gets its own log file derived from `{name}_{tag}`.
    let (name, tag) = (node.pin.name.as_str(), node.pin.tag.as_str());
    let log = ActionLog::for_add(
        &action_context.peppy_dirs,
        action_context.node_stack.log_exporter().clone(),
        &format!("{name}_{tag}"),
        Some((name, tag)),
        batch_goal.launch_id.as_deref(),
    )
    .map_err(|e| {
        Failure::new(
            PeppyFailureKind::AttemptFailed,
            format!("Failed to create sub-add log: {}", e),
        )
    })?;
    batch_announcer.line(format!("{name}:{tag} logs to {}", log.path().display()));

    // Mirror sub-add feedback onto the batch feedback channel so users see a
    // single stream of progress for the whole batch.
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let batch_fb = batch_announcer.feedback_tx().clone();
    let forwarder = tokio::spawn(async move {
        while let Some(line) = sub_rx.recv().await {
            let _ = batch_fb.send(line);
        }
    });

    let action_context = action_context.clone();
    let result = run_node_add(sub_goal, action_context, Announcer::new(log, sub_tx)).await;
    let _ = forwarder.await;
    result
}

/// Snapshot of a stack entity captured before the batch replaced it, so
/// rollback can re-install the prior config instead of removing the slot.
/// Artifact/stage state is intentionally omitted: a successful rollback
/// returns the entity to `Added` (pending build); any previously built
/// artifact remains on disk and a follow-up `node build` rewires it.
///
/// `working_dir` keeps the previous entity's `WorkingDirGuard` alive across
/// the sub-add: `config_path` points inside that temp dir, so without
/// holding the guard the directory would be removed when the sub-add
/// replaces the entity and rollback would re-install a config at a path
/// that no longer exists. `document_pins` are the pins of the documents the
/// previous entity's code was generated from, which the restored entity
/// records again.
struct PreviousConfig {
    config: NodeConfig,
    config_path: PathBuf,
    working_dir: Option<Arc<WorkingDirGuard>>,
    document_pins: Vec<PinnedItem>,
}

impl PreviousConfig {
    /// The snapshot of `entity`, taken before the batch replaces it.
    fn of(entity: &NodeEntity) -> Self {
        Self {
            config: entity.config().clone(),
            config_path: entity.config_path().to_path_buf(),
            working_dir: entity.pending_working_dir(),
            document_pins: entity.document_pins().to_vec(),
        }
    }
}

struct RollbackEntry {
    name: String,
    tag: String,
    previous: Option<PreviousConfig>,
}

/// Drop-based rollback: if the guard is dropped armed, every entry in
/// `added` is undone in reverse order (so dependants go first, then
/// deps). Entries that replaced an existing config restore the previous
/// state via `push_config`; entries that introduced a new slot are
/// removed. On success, call [`RollbackGuard::disarm`].
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
                previous,
            } = entry;
            match previous {
                Some(prev) => {
                    let PreviousConfig {
                        config,
                        config_path,
                        working_dir,
                        document_pins,
                    } = prev;
                    match self.node_stack.push_config(config, false, config_path) {
                        Ok(()) => {
                            // Reattach the previous working-dir guard and
                            // document pins, mirroring the post-`push_config`
                            // step in the add path: the restored entity owns
                            // the temp dir its `config_path` lives inside,
                            // and says which documents its code came from.
                            if let Some(handle) = self.node_stack.find(&name, &tag) {
                                let mut restored = handle.write();
                                if let Some(guard) = working_dir {
                                    restored.set_pending_working_dir(guard);
                                }
                                restored.set_document_pins(document_pins);
                            }
                            debug!("Rolled back batched replacement of {}:{}", name, tag)
                        }
                        Err(e) => warn!(
                            "Batch-add rollback (restore previous) failed for {}:{}: {}",
                            name, tag, e
                        ),
                    }
                }
                None => match self.node_stack.remove_config(&name, &tag) {
                    Ok(_) => debug!("Rolled back batched add of {}:{}", name, tag),
                    Err(e) => warn!("Batch-add rollback failed for {}:{}: {}", name, tag, e),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::repository::{
        EntryOrigin, GitCommit, ItemName, ItemTag, ManifestFingerprint, PinKind, PinnedItem,
        RepoRelativePath,
    };

    fn materialized(
        name: &str,
        tag: &str,
        deps: &[(&str, &str)],
        is_root: bool,
    ) -> MaterializedPin {
        let depends_on = if deps.is_empty() {
            String::new()
        } else {
            let list = deps
                .iter()
                .map(|(dep_name, dep_tag)| {
                    format!(r#"{{ name: "{dep_name}", tag: "{dep_tag}", link_id: "{dep_name}" }}"#)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(", depends_on: {{ nodes: [{list}] }}")
        };
        let config: NodeConfig = config::node::NodeConfigParser::from_content(&format!(
            r#"{{ peppy_schema: "node/v1",
                  manifest: {{ name: "{name}", tag: "{tag}"{depends_on} }},
                  execution: {{ language: "python", build_cmd: ["true"], run_cmd: ["true"] }} }}"#
        ))
        .expect("test config parses");
        MaterializedPin {
            pin: PinnedItem {
                kind: PinKind::Node,
                name: ItemName::parse(name).expect("valid name"),
                tag: ItemTag::parse(tag).expect("valid tag"),
                sha256: ManifestFingerprint::of_bytes(name.as_bytes()),
                origin: EntryOrigin::Git {
                    repo_url: "https://example.com/hub".to_owned(),
                    repo_ref: Some("main".to_owned()),
                    read_ref: Some(core_node_api::encoding::ReadRef::Named("main".to_owned())),
                    commit: GitCommit::parse(&"a".repeat(40)).expect("valid commit"),
                    path: RepoRelativePath::parse(&format!("{name}/peppy.json5"))
                        .expect("valid path"),
                },
            },
            root_dir: std::path::PathBuf::from("/unused"),
            config,
            is_root,
        }
    }

    /// A batch whose pins cover every declared dependency has no gaps, and
    /// a dependency satisfied by the stack (an earlier deployment of the
    /// same launch) is not a gap either.
    #[test]
    fn a_covered_closure_has_no_gaps() {
        let nodes = vec![
            materialized("camera", "v1", &[("driver", "v1"), ("planner", "v1")], true),
            materialized("driver", "v1", &[], false),
        ];
        let gaps = closure_gaps(&nodes, |name, _| name == "planner");
        assert!(gaps.is_empty(), "got: {gaps:?}");
    }

    /// The refusal this executor exists to give instead of a name lookup: a
    /// dependency in neither the pin set nor the stack names the gap and
    /// blames the pin set, never resolves the name.
    #[test]
    fn an_uncovered_dependency_is_an_incomplete_closure_refusal() {
        let nodes = vec![materialized("camera", "v1", &[("driver", "v2")], true)];
        let gaps = closure_gaps(&nodes, |_, _| false);
        assert_eq!(gaps.len(), 2, "a header plus one gap: {gaps:?}");
        assert!(gaps[0].contains("incomplete set"), "got: {gaps:?}");
        assert!(gaps[1].contains("`driver:v2`"), "got: {gaps:?}");
        assert!(gaps[1].contains("camera:v1"), "got: {gaps:?}");
    }

    /// A batch that fails after it replaced a node puts the previous node
    /// back with the pins of the documents its code was generated from, so
    /// `instance_documents` still answers for an instance of it.
    #[test]
    fn a_rolled_back_replacement_keeps_the_document_pins_of_the_previous_node() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = materialized("core_root", "v1", &[], false).config;
        let node_stack = Arc::new(node_stack::NodeStack::new(root, None, dir.path()));
        let camera = materialized("camera", "v1", &[], true);
        let previous_path = dir.path().join("previous");
        node_stack
            .push_config(camera.config.clone(), false, &previous_path)
            .expect("push the previous node");
        let contract_pin = PinnedItem {
            kind: PinKind::Contract,
            name: ItemName::parse("uvc_camera").expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: ManifestFingerprint::of_bytes(b"uvc_camera contract"),
            origin: camera.pin.origin.clone(),
        };
        let previous = {
            let handle = node_stack.find("camera", "v1").expect("the previous node");
            handle.write().set_document_pins(vec![contract_pin.clone()]);
            PreviousConfig::of(&handle.read())
        };
        // The add of the batch replaces the node with an entity of its own.
        node_stack
            .push_config(camera.config, false, dir.path().join("replacement"))
            .expect("the batch replaces the node");
        let mut rollback = RollbackGuard::new(Arc::clone(&node_stack));
        rollback.added.push(RollbackEntry {
            name: "camera".to_owned(),
            tag: "v1".to_owned(),
            previous: Some(previous),
        });

        drop(rollback);

        let restored = node_stack.find("camera", "v1").expect("the restored node");
        let restored = restored.read();
        assert_eq!(restored.config_path(), previous_path);
        assert_eq!(restored.document_pins(), [contract_pin]);
    }
}
