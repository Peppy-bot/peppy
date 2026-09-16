use config::AnyType;
use config::node::ImplementsEntry;
use config::runtime::{
    ClockBinding, ClockDomainId, CoreNodeName, Name, PairingSlotBinding,
    ProducerRef,
};
use core_node_api::encoding::{
    ClockDomainInfo, ClockListRequest, NodeInfoRequest, NodeInfoResponse, NodeRunFeedback,
    NodeRunGoal, NodeRunGoalResponse, NodeRunResult, ObservationTarget, ObservationTargets,
    PairTarget, StackListRequest,
};
use core_node_api::{ActionId, NodeStage};
use daemon_config::launcher::{
    BindingValidationItem, DeploymentInstance, LinkValue, PairingValidationItem, Placements,
    WALL_CLOCK, split_link_target, validate_link_plan,
};
use names_generator2::get_random;
use peppylib::core_node::transport::{poll, send_goal};
use peppylib::{CoreNodePresenceMessenger, MessengerHandle};
use rand::rng;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{debug, info};

use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::TimeoutConfig;
use super::env::caller_env_overrides;

/// Timeout for the quick `NodeInfoRequest` preflight in the `run -b` flow.
/// Matches `node info`'s request timeout; this is a metadata lookup,
/// not a long-running action, so it must fail fast if the daemon is down
/// rather than waiting out `timeouts.max_secs` (which can be 1 hour).
const NODE_INFO_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between `NodeInfoRequest` polls while waiting for an in-flight
/// build to transition `Building -> Ready`. Builds typically take
/// seconds-to-minutes, so 500 ms keeps CLI latency low without flooding the
/// daemon.
const BUILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What `run -b` should do with a node based on its current lifecycle stage.
#[derive(Debug, PartialEq, Eq)]
enum BuildDecision {
    /// Stage is `Ready`: artifact exists, skip the build and run directly.
    Skip,
    /// Stage is `Added`: trigger `build_node_async`.
    Build,
    /// Stage is `Building`: another build is in flight, poll until it
    /// finishes instead of trying to start a second build (the daemon
    /// rejects concurrent builds).
    Wait,
}

/// Pure helper: compute the remaining `max_secs` budget given how many
/// seconds have already elapsed. Split out from `remaining_timeouts` so the
/// budget-arithmetic + error path can be unit-tested without needing a
/// tokio runtime or time-mocking feature.
fn remaining_max_secs(original_max: u64, elapsed: u64, stage: &str) -> Result<u64> {
    if elapsed >= original_max {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {original_max}s exceeded before {stage}. \
             Use --max-timeout <seconds> to increase."
        )));
    }
    Ok(original_max - elapsed)
}

/// Derive a new `TimeoutConfig` whose `max_secs` is what remains of the
/// original `max_secs` budget after the time elapsed since `start`. Returns
/// an `ExecutionFailed` error if no budget remains, so callers never kick
/// off a stage with a zero deadline.
///
/// `idle_secs` is preserved as-is: it's a per-call "no output" guard, not a
/// wall-clock budget, so it should not shrink across stages.
fn remaining_timeouts(
    timeouts: &TimeoutConfig,
    start: Instant,
    stage: &str,
) -> Result<TimeoutConfig> {
    let elapsed = start.elapsed().as_secs();
    let max_secs = remaining_max_secs(timeouts.max_secs, elapsed, stage)?;
    Ok(TimeoutConfig {
        idle_secs: timeouts.idle_secs,
        max_secs,
    })
}

/// Classify a node's lifecycle stage into a `run -b` action.
///
/// Split out as a pure function so the stage-matching logic can be
/// unit-tested directly.
fn classify_stage(stage: NodeStage, node_name: &str, tag: &str) -> Result<BuildDecision> {
    match stage {
        NodeStage::Ready => Ok(BuildDecision::Skip),
        NodeStage::Added => Ok(BuildDecision::Build),
        NodeStage::Building => Ok(BuildDecision::Wait),
        NodeStage::Root => Err(Error::ExecutionFailed(format!(
            "Node '{}:{}' is a root node and cannot be built or run via `node run`",
            node_name, tag
        ))),
    }
}

/// Polls `NodeInfoRequest` until the node's stage transitions out of
/// `Building`. Returns `Ok(())` when the stage becomes `Ready`; returns an
/// error on timeout, on unexpected stage transitions (e.g. a failed build
/// falling back to `Added`), or if the node disappears from the stack.
async fn wait_for_build_to_finish(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    timeouts: &TimeoutConfig,
) -> Result<()> {
    info!(
        "Node {}:{} is already building, waiting for the in-flight build to finish...",
        node_name, tag
    );

    let deadline = Instant::now() + Duration::from_secs(timeouts.max_secs);

    loop {
        let response = poll(
            &NodeInfoRequest::new(node_name.to_string(), tag.to_string()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to poll node info while waiting for build: {}",
                e
            ))
        })?;

        match response {
            NodeInfoResponse::NotInStack => {
                return Err(Error::ExecutionFailed(format!(
                    "Node '{}:{}' disappeared from the stack while waiting for build to finish",
                    node_name, tag
                )));
            }
            NodeInfoResponse::Found(info) => match info.stage {
                NodeStage::Ready => return Ok(()),
                NodeStage::Building => { /* still in flight; keep polling */ }
                other => {
                    return Err(Error::ExecutionFailed(format!(
                        "Node '{}:{}' transitioned to unexpected stage '{}' while waiting for build to finish",
                        node_name, tag, other
                    )));
                }
            },
        }

        if Instant::now() >= deadline {
            return Err(Error::ExecutionFailed(format!(
                "Timed out waiting for node '{}:{}' build to finish",
                node_name, tag
            )));
        }

        sleep(BUILD_WAIT_POLL_INTERVAL).await;
    }
}

/// Converts a list of key=value string pairs into node arguments.
/// Dot-separated keys are converted into nested objects.
/// For example: "device.physical=/dev/video0" becomes {"device": {"physical": "/dev/video0"}}
///
/// Values are parsed with type inference:
/// - "true"/"false" -> Bool
/// - Integer strings -> Int
/// - Float strings -> Float
/// - Everything else -> String
pub fn args_to_node_arguments(args: &[(String, String)]) -> BTreeMap<String, AnyType> {
    let mut result: BTreeMap<String, AnyType> = BTreeMap::new();

    for (key, value) in args {
        let parsed_value = parse_value(value);
        insert_nested_value(&mut result, key, parsed_value);
    }

    result
}

/// Inserts a value at a dot-separated path into a nested BTreeMap structure.
/// For example, path "device.physical" with value "foo" creates:
/// {"device": {"physical": "foo"}}
fn insert_nested_value(
    root: &mut std::collections::BTreeMap<String, AnyType>,
    path: &str,
    value: AnyType,
) {
    let parts: Vec<&str> = path.split('.').collect();

    if parts.len() == 1 {
        // Simple key, insert directly
        root.insert(path.to_string(), value);
        return;
    }

    // For nested paths, we need to navigate/create the path
    insert_at_path(root, &parts, value);
}

/// Recursively inserts a value at the given path parts.
fn insert_at_path(
    current: &mut std::collections::BTreeMap<String, AnyType>,
    parts: &[&str],
    value: AnyType,
) {
    if parts.is_empty() {
        return;
    }

    let key = parts[0].to_string();

    if parts.len() == 1 {
        // Last part - insert the actual value
        current.insert(key, value);
        return;
    }

    // Intermediate part - ensure an Object exists at this key
    let entry = current
        .entry(key)
        .or_insert_with(|| AnyType::Object(std::collections::BTreeMap::new()));

    // If the entry isn't an object, make it one
    if !matches!(entry, AnyType::Object(_)) {
        *entry = AnyType::Object(std::collections::BTreeMap::new());
    }

    // Navigate into the object and recurse
    if let AnyType::Object(obj) = entry {
        insert_at_path(obj, &parts[1..], value);
    }
}

/// Parses a string value into an AnyType with type inference
fn parse_value(value: &str) -> AnyType {
    // Try bool
    if value.eq_ignore_ascii_case("true") {
        return AnyType::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return AnyType::Bool(false);
    }

    // Try integer (i64)
    if let Ok(int_val) = value.parse::<i64>() {
        return AnyType::Int(int_val);
    }

    // Try float (f64)
    if let Ok(float_val) = value.parse::<f64>() {
        return AnyType::Float(float_val);
    }

    // Default to string
    AnyType::String(value.to_string())
}

/// A clock an instance is told to read: `wall`, a domain name, or a name
/// qualified by the machine hosting it when one name runs on several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockReference {
    pub name: Name,
    pub core_node: Option<CoreNodeName>,
}

/// Parses `--clock`: `wall`, `robot`, or `robot@cn-sim`. The shape is checked
/// here; whether the domain exists is answered by the daemons that host them.
pub(super) fn parse_clock_reference(value: &str) -> std::result::Result<ClockReference, String> {
    let (name, core_node) = match value.split_once('@') {
        None => (value, None),
        Some((name, machine)) => (
            name,
            Some(
                CoreNodeName::new(machine)
                    .map_err(|reason| format!("`{machine}` is not a core node name: {reason}"))?,
            ),
        ),
    };
    Ok(ClockReference {
        name: Name::new(name)
            .map_err(|reason| format!("`{name}` is not a clock domain name: {reason}"))?,
        core_node,
    })
}

/// Parses `--publish-clock`: the name of the domain this instance supplies.
pub(super) fn parse_clock_domain_name(value: &str) -> std::result::Result<Name, String> {
    Name::new(value).map_err(|reason| format!("`{value}` is not a clock domain name: {reason}"))
}

/// What the clock flags asked for. Both absent is wall time, which is what an
/// instance whose deployment names no domain reads.
#[derive(Clone, Debug, Default)]
pub struct ClockChoice {
    pub clock: Option<ClockReference>,
    pub publish_clock: Option<Name>,
}

/// What the fan-out over the federation found for a `--clock` reference: the
/// domains matching it, and the daemons that did not answer.
#[derive(Debug, Default)]
struct ClockSearch {
    matches: Vec<ClockDomainInfo>,
    unreachable: Vec<String>,
}

impl ClockSearch {
    /// The peers that did not answer, as a refusal names them.
    fn unreachable_clause(&self) -> String {
        format!(
            "{} daemon(s) did not answer ({})",
            self.unreachable.len(),
            self.unreachable.join(", ")
        )
    }
}

/// The binding `--publish-clock` declares: a fresh lifetime of `name`, hosted
/// by the daemon this command targets.
fn publisher_binding(name: &Name, target_core_node: &str) -> Result<ClockBinding> {
    if name.as_str() == WALL_CLOCK {
        return Err(Error::ExecutionFailed(format!(
            "`{WALL_CLOCK}` is built in and names the time every machine already keeps, so it \
             cannot be declared. An instance reads it wherever `--clock` names nothing else; \
             supply a domain of your own under another name, such as `--publish-clock robot_sim`"
        )));
    }
    let core_node = CoreNodeName::new(target_core_node).map_err(|reason| {
        Error::ExecutionFailed(format!(
            "the target daemon reports an invalid core node name `{target_core_node}`: {reason}"
        ))
    })?;
    Ok(ClockBinding::publisher(ClockDomainId::new(
        name.clone(),
        core_node,
        daemon_config::launcher::mint_incarnation(),
    )))
}

/// Whether a `--clock` reference names wall time, which every machine keeps
/// and no daemon hosts.
fn refers_to_wall(reference: &ClockReference) -> Result<bool> {
    if reference.name.as_str() != WALL_CLOCK {
        return Ok(false);
    }
    let Some(machine) = &reference.core_node else {
        return Ok(true);
    };
    Err(Error::ExecutionFailed(format!(
        "`{WALL_CLOCK}` is built in and names the time every machine already keeps, so \
         `{WALL_CLOCK}@{machine}` names nothing one machine supplies to another. Name it as \
         `--clock {WALL_CLOCK}`"
    )))
}

/// The binding a `--clock` reference resolves to, given what the federation
/// answered.
///
/// A daemon that did not answer may host the very name being resolved, so a
/// short name is settled only by a complete answer: resolving one against a
/// partial answer binds whichever machine happened to reply. A reference that
/// names its machine is unambiguous on its own, and resolves on what came
/// back.
fn consumer_binding(reference: &ClockReference, found: &ClockSearch) -> Result<ClockBinding> {
    match found.matches.as_slice() {
        [] if found.unreachable.is_empty() => Err(Error::ExecutionFailed(format!(
            "no clock domain `{name}` is running; `peppy clock list` shows the domains this \
             federation hosts, or declare one with `--publish-clock {name}`",
            name = reference.name
        ))),
        [] => Err(Error::ExecutionFailed(format!(
            "no clock domain `{name}` was found, and {clause}, so whether one is running is \
             unknown. `peppy clock list` reports the same gap; retry once every daemon answers",
            name = reference.name,
            clause = found.unreachable_clause()
        ))),
        [one] if reference.core_node.is_some() || found.unreachable.is_empty() => {
            Ok(ClockBinding::consumer(
                one.domain.clone(),
                ProducerRef::new(one.domain.core_node.as_str(), &one.publisher_instance_id),
            ))
        }
        [one] => Err(Error::ExecutionFailed(format!(
            "`{name}` matched `{domain}`, and {clause}, so another machine may host the same \
             name. Name the one you mean as `--clock {domain}`, or retry once every daemon \
             answers",
            name = reference.name,
            domain = one.domain,
            clause = found.unreachable_clause()
        ))),
        several => Err(Error::ExecutionFailed(format!(
            "`{}` names more than one clock domain ({}); name the one you mean, machine \
             included",
            reference.name,
            several
                .iter()
                .map(|info| info.domain.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Asks every live daemon what it hosts, keeping the domains that match
/// `reference` and the peers that did not answer.
async fn search_federation(
    messenger: &MessengerHandle,
    caller_core_node: &str,
    reference: &ClockReference,
) -> Result<ClockSearch> {
    let live = CoreNodePresenceMessenger::list_live(
        messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await
    .map_err(|error| {
        Error::ExecutionFailed(format!("could not enumerate the federation: {error}"))
    })?;

    let mut found = ClockSearch::default();
    for claim in live {
        let answer = poll::<ClockListRequest>(
            &ClockListRequest::new(),
            messenger,
            caller_core_node,
            CALLER_INSTANCE_ID,
            &claim.core_node,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await;
        let Ok(response) = answer else {
            found.unreachable.push(claim.core_node);
            continue;
        };
        found
            .matches
            .extend(response.domains.into_iter().filter(|info| {
                info.domain.name == reference.name
                    && reference
                        .core_node
                        .as_ref()
                        .is_none_or(|machine| &info.domain.core_node == machine)
            }));
    }
    Ok(found)
}

/// Turns the clock flags into the binding this instance starts with.
///
/// `--publish-clock` declares the domain on the daemon this command targets
/// and mints its lifetime. `--clock` names one that already runs: every live
/// daemon is asked what it hosts, and the reference binds only on an answer
/// that settles it. An unknown name, a name more than one machine runs, and a
/// name an incomplete answer leaves in doubt are each refused.
pub(super) async fn resolve_clock_choice(
    messenger: &MessengerHandle,
    caller_core_node: &str,
    target_core_node: &str,
    choice: &ClockChoice,
) -> Result<ClockBinding> {
    if let Some(name) = &choice.publish_clock {
        return publisher_binding(name, target_core_node);
    }
    let Some(reference) = &choice.clock else {
        return Ok(ClockBinding::Wall);
    };
    if refers_to_wall(reference)? {
        return Ok(ClockBinding::Wall);
    }
    let found = search_federation(messenger, caller_core_node, reference).await?;
    consumer_binding(reference, &found)
}

/// The resolved preflight plan for one instance's `--link` flags: the
/// producer-binding slots resolved to concrete producer sets, the
/// participant-pairing links extracted into the `PairTarget` map the daemon's
/// `node_run` re-plans, and the observer links resolved into the
/// `ObservationTargets` map the daemon registers with its observation
/// coordinator. Carrying observations on the goal is what makes a lone
/// `node run` observer receive its source pin exactly like a launcher would;
/// without it the observer would boot validated but silent.
#[derive(Default)]
struct PreflightPlan {
    slot_bindings: config::runtime::SlotBindings,
    requested_pairs: BTreeMap<String, PairTarget>,
    vacant_pairs: BTreeMap<String, String>,
    requested_observations: BTreeMap<String, ObservationTargets>,
}

/// Pre-flight bind validation. Snapshots the running stack via
/// `stack_list` + `node_info`, feeds it together with the consumer
/// being launched into the launcher's `validate_bindings`, and returns
/// the resolved per-slot producer map for the consumer instance.
/// Every rule violation is a hard error; there is no warning path.
///
/// The snapshot is split into two flavors of [`BindingValidationItem`]:
///
/// - **Inert items**: one per already-running `(name, tag)` group. They
///   carry real `instances` (so stack-wide `instance_id` uniqueness can
///   fire) and real `implements` (so the new instance can still match
///   them as a producer / contract-implementing target), but their
///   `depends_on` is `None`. Their declared slots were already resolved
///   when each instance was first spawned; forwarding the real
///   `depends_on` here (with the empty `bindings` we synthesize) would
///   pit the every-slot-bound rule against instances whose real bindings
///   live in their own boot configs.
/// - **One live item**: for the synthesized new instance, carrying the
///   target's real `depends_on` + `implements`. This is the only item
///   whose bindings are validated and materialized.
///
/// Returns `Ok(None)` on transient transport failures so the call site
/// can swallow them and continue; an unreachable daemon should fail
/// the actual `node_run` invocation, not the pre-flight.
async fn validate_links_against_stack(
    messenger: &MessengerHandle,
    core_node_name: &str,
    target_name: &str,
    target_tag: &str,
    target_instance_id: &str,
    links: &BTreeMap<String, LinkValue>,
    clock: &ClockBinding,
) -> Result<Option<PreflightPlan>> {
    let stack_response = poll(
        &StackListRequest::new(),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        NODE_INFO_PREFLIGHT_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("failed to list stack: {e}")))?;

    let graph = crate::commands::parse_stack_graph(&stack_response.graph_json)?;

    /// Inert snapshot entry for an already-running `(name, tag)` group.
    /// Note the missing `depends_on` field: by construction these items
    /// are deliberately decoupled from per-instance binding rules; see
    /// the function-level doc above.
    struct StackNode {
        name: String,
        tag: String,
        instances: Vec<DeploymentInstance>,
        implements: Vec<ImplementsEntry>,
        pairing_deps: Vec<config::node::PairingParticipantDependency>,
        observer_deps: Vec<config::node::PairingObserverDependency>,
    }

    let stack_nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| !matches!(n.stage, NodeStage::Root))
        .collect();

    let info_futures = stack_nodes.iter().map(|node| async move {
        let info = poll(
            &NodeInfoRequest::new(node.name.clone(), node.tag.clone()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "failed to fetch info for stack node '{}:{}': {e}",
                node.name, node.tag
            ))
        })?;
        Ok::<_, Error>(info)
    });
    let infos = futures::future::try_join_all(info_futures).await?;

    // `(depends_on, implements)` for the target node, harvested from
    // the snapshot if the target is already in the stack so we can
    // avoid a second `node_info` round-trip. Falls back to `None` /
    // empty when the target hasn't been added yet (also covers transient
    // misses below).
    let mut target_depends_on: Option<config::node::DependsOn> = None;
    let mut target_implements: Vec<ImplementsEntry> = Vec::new();
    let mut target_seen_in_stack = false;

    // Pairing slots of running instances that are exclusively claimed right
    // now, fed to `validate_pairings` so a `--link` at a taken slot fails
    // in the preflight with the existing peer named.
    let mut already_paired = daemon_config::launcher::AlreadyPairedSlots::new();

    // The clock each running instance reads, so this instance's connections
    // are held to the same rule a launch applies: both ends of a
    // clock-dependent connection read one clock.
    let mut running_clocks: Vec<(String, ClockBinding)> = Vec::new();

    let mut snapshot: Vec<StackNode> = Vec::with_capacity(stack_nodes.len());
    for (node, info_response) in stack_nodes.iter().zip(infos) {
        let info = match info_response {
            NodeInfoResponse::Found(info) => info,
            NodeInfoResponse::NotInStack => continue,
        };
        for inst in &info.instances {
            running_clocks.push((inst.instance_id.clone(), inst.clock.clone()));
            for (link_id, slot) in &inst.pairing_slots {
                if let PairingSlotBinding::Paired { peer, peer_link_id } = &slot.binding {
                    already_paired.insert(
                        (inst.instance_id.clone(), link_id.clone()),
                        format!("{}:{}", peer.instance_id, peer_link_id),
                    );
                }
            }
        }
        // Harvest the target's manifest/interfaces from its snapshot
        // entry so we don't need a second `node_info` call below.
        if node.name == target_name && node.tag == target_tag {
            target_depends_on = info.config.manifest.depends_on.clone();
            target_implements = info.config.manifest.implements.clone();
            target_seen_in_stack = true;
        }
        // The validator reads `instance_id` and `bindings` for inert items
        // (`bindings` is unused under `depends_on: None`, but kept empty to
        // satisfy the type). `arguments`, `env_vars` and `framework` are not
        // consulted: what clock a running instance reads is reported by the
        // daemon, above, rather than re-derived from a deployment entry this
        // command never saw.
        let instances: Vec<DeploymentInstance> = node
            .instances
            .iter()
            .filter(|inst| inst.state == core_node_api::InstanceState::Running)
            .filter_map(|inst| {
                Name::new(inst.instance_id.clone())
                    .ok()
                    .map(DeploymentInstance::empty)
            })
            .collect();
        snapshot.push(StackNode {
            name: node.name.clone(),
            tag: node.tag.clone(),
            instances,
            implements: info.config.manifest.implements.clone(),
            pairing_deps: info
                .config
                .manifest
                .depends_on
                .as_ref()
                .map(|d| d.pairings.clone())
                .unwrap_or_default(),
            observer_deps: info
                .config
                .manifest
                .depends_on
                .map(|d| d.pairing_observers)
                .unwrap_or_default(),
        });
    }

    // Target wasn't in the stack snapshot (e.g., the user is launching
    // the only instance of a freshly-added node). Fall back to a direct
    // `node_info` lookup so the validator can still resolve dead-key /
    // missing-binding rules against the target's declared manifest.
    if !target_seen_in_stack {
        let info_response = poll(
            &NodeInfoRequest::new(target_name.to_owned(), target_tag.to_owned()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|r| match r {
            NodeInfoResponse::Found(info) => Some(info),
            NodeInfoResponse::NotInStack => None,
        });
        if let Some(info) = info_response {
            target_depends_on = info.config.manifest.depends_on;
            target_implements = info.config.manifest.implements;
        }
    }

    // The one live item: the synthesized new instance with the target's
    // real `depends_on` + `implements`. Lives in its own group so it
    // never inherits the inert `depends_on: None` of an existing target
    // group.
    let synthetic_instances = vec![DeploymentInstance {
        links: links.clone(),
        ..DeploymentInstance::empty(
            Name::new(target_instance_id.to_owned()).map_err(|e| Error::PeppyConfig(e.into()))?,
        )
    }];

    let mut items: Vec<BindingValidationItem<'_>> = snapshot
        .iter()
        .map(|s| BindingValidationItem {
            node_name: &s.name,
            node_tag: &s.tag,
            instances: &s.instances,
            depends_on: None,
            implements: &s.implements,
        })
        .collect();
    items.push(BindingValidationItem {
        node_name: target_name,
        node_tag: target_tag,
        instances: &synthetic_instances,
        depends_on: target_depends_on.as_ref(),
        implements: &target_implements,
    });

    // Build the pairing/observation view over the same snapshot. Running
    // instances are valid targets but exempt from coverage (their slots were
    // covered at their own start).
    let target_pairing_deps: Vec<config::node::PairingParticipantDependency> = target_depends_on
        .as_ref()
        .map(|d| d.pairings.clone())
        .unwrap_or_default();
    let target_observer_deps: Vec<config::node::PairingObserverDependency> = target_depends_on
        .as_ref()
        .map(|d| d.pairing_observers.clone())
        .unwrap_or_default();
    let mut pairing_items: Vec<PairingValidationItem<'_>> = snapshot
        .iter()
        .map(|s| PairingValidationItem {
            node_name: &s.name,
            node_tag: &s.tag,
            instances: &s.instances,
            pairing_deps: &s.pairing_deps,
            observer_deps: &s.observer_deps,
            preexisting: true,
        })
        .collect();
    pairing_items.push(PairingValidationItem {
        node_name: target_name,
        node_tag: target_tag,
        instances: &synthetic_instances,
        pairing_deps: &target_pairing_deps,
        observer_deps: &target_observer_deps,
        preexisting: false,
    });
    let mut clocks = daemon_config::launcher::ResolvedClocks::of_running(running_clocks);
    clocks.insert(target_instance_id, clock.clone());
    let mut validated = validate_link_plan(
        &items,
        &pairing_items,
        &already_paired,
        // A `node run` preflight sees the whole stack it validates against, so
        // nothing is covered outside the validator's view.
        &daemon_config::launcher::ExternallyCoveredSlots::new(),
        // `node run` targets one daemon, so every instance it can see is on it.
        &Placements::all_on(CoreNodeName::new(core_node_name).map_err(|reason| {
            Error::ExecutionFailed(format!(
                "the target daemon reports an invalid core node name `{core_node_name}`: {reason}"
            ))
        })?),
        &clocks,
    );
    if !validated.errors.is_empty() {
        let errors: Vec<String> = validated.errors.iter().map(ToString::to_string).collect();
        return Err(Error::ExecutionFailed(daemon_config::format_bulleted(
            &errors,
        )));
    }

    // Extract the participant-pairing links into the `PairTarget` map the goal
    // carries: the daemon's `node_run` re-plans them exactly as a launcher
    // deployment would. A participant link's single scalar target parses into
    // `<peer_instance>[/<peer_link_id>]`. Observer links produce no goal state.
    let participant_link_ids: std::collections::BTreeSet<&str> = target_pairing_deps
        .iter()
        .map(|d| d.link_id.as_str())
        .collect();
    // A vacancy on a participant slot rides to the daemon as the reason it
    // carries; observer vacancies produce no goal state, exactly as observer
    // links do, and a producer vacancy rides as the empty set its resolved
    // `slot_bindings` entry carries rather than as a reason.
    let vacant_pairs = daemon_config::launcher::participant_vacancies(links, &participant_link_ids);
    let mut requested_pairs: BTreeMap<String, PairTarget> = BTreeMap::new();
    for (link_id, value) in links {
        if !participant_link_ids.contains(link_id.as_str()) {
            continue;
        }
        let Some(selection) = value.selection() else {
            continue;
        };
        // Scalar-ness was already enforced by `validate_pairings`; a
        // participant link that survived it is a single target.
        if let Some(target) = selection.as_scalar() {
            let (peer_instance, peer_link) = split_link_target(target);
            let pair_target = match peer_link {
                Some(link) => PairTarget::pinned(peer_instance, link, core_node_name),
                None => PairTarget::new(peer_instance, core_node_name),
            };
            requested_pairs.insert(link_id.clone(), pair_target);
        }
    }

    // Extract the observer links into the `ObservationTargets` map the goal
    // carries, keyed by the observer's own slot link_id and holding that slot's
    // whole member set in `--link` occurrence order. Only observations for THIS
    // instance go on its goal (a preexisting instance's observers were
    // registered at its own start); the daemon re-stamps the source core_node,
    // so it is dropped here exactly as a pair target drops it.
    let mut observation_members: BTreeMap<String, Vec<ObservationTarget>> = BTreeMap::new();
    for observation in validated
        .planned_observations
        .iter()
        .filter(|obs| obs.observer_instance_id == target_instance_id)
    {
        observation_members
            .entry(observation.observer_link_id.clone())
            .or_default()
            .push(ObservationTarget::new(
                &observation.source.instance_id,
                &observation.source_link_id,
                core_node_name,
            ));
    }
    let requested_observations = ObservationTargets::slots_from_plan(observation_members);

    Ok(Some(PreflightPlan {
        slot_bindings: validated
            .slot_bindings
            .remove(target_instance_id)
            .unwrap_or_default(),
        requested_pairs,
        vacant_pairs,
        requested_observations,
    }))
}

/// Validate the supplied `--link` entries against the running stack, resolve
/// them to per-slot producer lists, then spawn the instance. This is the
/// single entry point shared between `peppy node run` and `peppy node add
/// --run`: both surfaces must accept `--link` and enforce the same binding
/// rules, so there is exactly one code path responsible for materializing
/// the instance_id, running `validate_bindings`, and calling
/// [`run_instance_async`].
///
/// `instance_id` is materialized up-front so the synthetic
/// `DeploymentInstance` fed to the validator and the actual spawn refer to
/// the same id; mismatching them would point validator errors at a
/// different instance than the one that ends up running.
#[allow(clippy::too_many_arguments)]
pub async fn validate_and_run_instance(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
    links: &BTreeMap<String, LinkValue>,
    clock: ClockBinding,
    timeouts: &TimeoutConfig,
) -> Result<String> {
    let prelaunch_instance_id = instance_id.unwrap_or_else(|| get_random(rng()));

    // A `None` plan means the preflight could not reach the daemon. We cannot
    // classify links without the target's manifest, so nothing is pre-resolved
    // and the goal carries no pairs. In practice this path is inert: a daemon
    // unreachable at preflight also fails the `node_run` goal send below, so
    // the user sees a clear transport error rather than a bad boot.
    let plan = match validate_links_against_stack(
        messenger,
        core_node_name,
        node_name,
        tag,
        &prelaunch_instance_id,
        links,
        &clock,
    )
    .await
    {
        Ok(Some(plan)) => plan,
        Ok(None) => PreflightPlan::default(),
        Err(e @ Error::ExecutionFailed(_)) => return Err(e),
        Err(e) => {
            debug!("skipping link validation for {}:{}: {}", node_name, tag, e);
            PreflightPlan::default()
        }
    };

    run_instance_async(
        messenger,
        core_node_name,
        node_name,
        tag,
        args,
        Some(prelaunch_instance_id),
        plan.slot_bindings,
        plan.requested_pairs,
        plan.vacant_pairs,
        plan.requested_observations,
        clock,
        timeouts,
    )
    .await
}

/// Spawn a node instance with already-resolved `slot_bindings`. Callers must
/// have validated `binds` via [`validate_and_run_instance`] first; invoking
/// this directly bypasses every binding rule and exists only as the lower
/// half of the validate-then-spawn split.
///
/// `core_node_name` plays both roles (caller identity and goal target) because
/// this path routes to the local daemon only; the entry points enforce that via
/// `reject_remote_target_for_local_routing`. Nothing on the goal is host-local
/// any more, so the restriction is about routing rather than about the payload.
#[allow(clippy::too_many_arguments)]
pub async fn run_instance_async(
    messenger_handle: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
    slot_bindings: config::runtime::SlotBindings,
    requested_pairs: BTreeMap<String, PairTarget>,
    vacant_pairs: BTreeMap<String, String>,
    requested_observations: BTreeMap<String, ObservationTargets>,
    clock: ClockBinding,
    timeouts: &TimeoutConfig,
) -> Result<String> {
    // Generate or use provided instance_id
    let instance_id = instance_id.unwrap_or_else(|| get_random(rng()));

    // Convert CLI arguments to node arguments
    let arguments = args_to_node_arguments(args);

    info!(
        "Starting node {}:{} with instance_id '{}' and {} argument(s)...",
        node_name,
        tag,
        instance_id,
        arguments.len()
    );

    // A PLAN, not a config. The CLI says WHAT to start; the daemon owns the
    // node's runtime identity and assembles the rest from its own state.
    //
    // This is what removes `node run`'s local-only restriction: the CLI used to
    // bake its own session's messaging endpoint and core node into the config
    // it shipped, so aiming the command at another daemon produced a node bound
    // to the wrong machine. With nothing host-local on the goal there is
    // nothing left to get wrong.
    let instance_plan = config::runtime::NodeInstancePlan {
        arguments,
        clock,
        slot_bindings,
        ..config::runtime::NodeInstancePlan::new(
            Name::new(instance_id.clone()).map_err(|e| Error::PeppyConfig(e.into()))?,
        )
    };

    info!(
        "Calling node_run for {}:{} (instance_id={})...",
        node_name, tag, instance_id
    );

    let start_goal = NodeRunGoal::new(
        instance_plan,
        node_name.to_string(),
        tag.to_string(),
        timeouts.max_secs,
    )
    .with_env_vars(caller_env_overrides())
    .with_requested_pairs(requested_pairs)
    .with_vacant_pairs(vacant_pairs)
    .with_planned_observations(requested_observations);
    let mut action_handle = send_goal(
        &start_goal,
        messenger_handle,
        core_node_name,
        CALLER_INSTANCE_ID,
        Some(core_node_name),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_run goal: {}", e)))?;

    let start_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeRunGoalResponse,
        NodeRunFeedback,
        NodeRunResult,
    >(
        messenger_handle,
        &mut action_handle,
        timeouts,
        ActionId::NodeRun.name(),
    )
    .await?;

    if let Some(pid) = start_result.pid {
        info!("Started node instance '{}' (pid: {})", instance_id, pid);
    } else {
        info!("Started node instance '{}'", instance_id);
    }
    Ok(instance_id)
}

#[allow(clippy::too_many_arguments)]
pub fn run_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    clock: ClockChoice,
    links: BTreeMap<String, LinkValue>,
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    crate::commands::block_on(run_node_async(
        ctx,
        node_name,
        tag,
        args,
        instance_id,
        clock,
        links,
        timeouts,
        build,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    clock: ClockChoice,
    links: BTreeMap<String, LinkValue>,
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // Every step below addresses `conn.core_node_name`, this machine's daemon,
    // so an override would name a machine nothing actually runs on.
    crate::commands::reject_remote_target_for_local_routing(&conn, "peppy node run")?;

    // Single end-to-end budget: every subsequent blocking stage (build, wait,
    // run) derives its `max_secs` from what's left of this budget, so the sum
    // of their wall-clock deadlines cannot exceed the original
    // `timeouts.max_secs`. The preflight `NodeInfoRequest` below intentionally
    // uses its own fixed-30s timeout and is exempt (see
    // `NODE_INFO_PREFLIGHT_TIMEOUT` docs above).
    let start = Instant::now();

    if build {
        // Look up the node's current lifecycle stage so we only trigger a
        // build when the node is not yet built. The same `NodeInfoRequest`
        // is used by the `node add` preflight (see add.rs).
        let response = poll(
            &NodeInfoRequest::new(node_name.clone(), tag.clone()),
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to check node info before run: {}", e))
        })?;

        let info = match response {
            NodeInfoResponse::NotInStack => {
                return Err(Error::ExecutionFailed(format!(
                    "Node '{}:{}' is not in the node stack",
                    node_name, tag
                )));
            }
            NodeInfoResponse::Found(info) => info,
        };

        match classify_stage(info.stage, &node_name, &tag)? {
            BuildDecision::Skip => {
                info!(
                    "Node {}:{} has already been built, skipping build",
                    node_name, tag
                );
            }
            BuildDecision::Build => {
                super::builder::build_node_async(
                    conn.messenger,
                    &conn.core_node_name,
                    &node_name,
                    &tag,
                    &remaining_timeouts(&timeouts, start, "build")?,
                    super::BuildOptions::default(),
                )
                .await?;
            }
            BuildDecision::Wait => {
                wait_for_build_to_finish(
                    conn.messenger,
                    &conn.core_node_name,
                    &node_name,
                    &tag,
                    &remaining_timeouts(&timeouts, start, "wait-for-build")?,
                )
                .await?;
            }
        }
    }

    // Resolved before the instance starts: an unknown domain is a refusal
    // here, where nothing has been spawned yet.
    let clock = resolve_clock_choice(
        conn.messenger,
        &conn.core_node_name,
        &conn.target_core_node,
        &clock,
    )
    .await?;
    validate_and_run_instance(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &args,
        instance_id,
        &links,
        clock,
        &remaining_timeouts(&timeouts, start, "run")?,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the fixtures below build a lifetime by hand; the command mints
    // through `daemon_config`.
    use config::runtime::ClockIncarnation;

    #[test]
    fn parse_bool_values() {
        assert_eq!(parse_value("true"), AnyType::Bool(true));
        assert_eq!(parse_value("True"), AnyType::Bool(true));
        assert_eq!(parse_value("TRUE"), AnyType::Bool(true));
        assert_eq!(parse_value("false"), AnyType::Bool(false));
        assert_eq!(parse_value("False"), AnyType::Bool(false));
        assert_eq!(parse_value("FALSE"), AnyType::Bool(false));
    }

    #[test]
    fn parse_int_values() {
        assert_eq!(parse_value("42"), AnyType::Int(42));
        assert_eq!(parse_value("-42"), AnyType::Int(-42));
        assert_eq!(parse_value("0"), AnyType::Int(0));
    }

    #[test]
    fn parse_float_values() {
        assert_eq!(parse_value("1.25"), AnyType::Float(1.25));
        assert_eq!(parse_value("-2.5"), AnyType::Float(-2.5));
        assert_eq!(parse_value("0.0"), AnyType::Float(0.0));
    }

    #[test]
    fn parse_string_values() {
        assert_eq!(parse_value("hello"), AnyType::String("hello".to_string()));
        assert_eq!(
            parse_value("1280x720"),
            AnyType::String("1280x720".to_string())
        );
        assert_eq!(
            parse_value("foo=bar"),
            AnyType::String("foo=bar".to_string())
        );
    }

    #[test]
    fn args_to_node_arguments_converts_correctly() {
        let args = vec![
            ("resolution".to_string(), "1280x720".to_string()),
            ("frequency".to_string(), "30".to_string()),
            ("enabled".to_string(), "true".to_string()),
            ("gain".to_string(), "1.5".to_string()),
        ];

        let node_args = args_to_node_arguments(&args);

        assert_eq!(node_args.len(), 4);
        assert_eq!(
            node_args.get("resolution"),
            Some(&AnyType::String("1280x720".to_string()))
        );
        assert_eq!(node_args.get("frequency"), Some(&AnyType::Int(30)));
        assert_eq!(node_args.get("enabled"), Some(&AnyType::Bool(true)));
        assert_eq!(node_args.get("gain"), Some(&AnyType::Float(1.5)));
    }

    #[test]
    fn args_to_node_arguments_handles_nested_keys() {
        let args = vec![
            ("device.physical".to_string(), "/dev/video0".to_string()),
            ("device.sim".to_string(), "mock:camera".to_string()),
            ("video.frame_rate".to_string(), "30".to_string()),
            ("video.resolution.width".to_string(), "1280".to_string()),
            ("video.resolution.height".to_string(), "720".to_string()),
        ];

        let node_args = args_to_node_arguments(&args);

        // Should have 2 top-level keys: device and video
        assert_eq!(node_args.len(), 2);

        // Check device object
        let device = node_args.get("device").expect("device should exist");
        match device {
            AnyType::Object(device_obj) => {
                assert_eq!(device_obj.len(), 2);
                assert_eq!(
                    device_obj.get("physical"),
                    Some(&AnyType::String("/dev/video0".to_string()))
                );
                assert_eq!(
                    device_obj.get("sim"),
                    Some(&AnyType::String("mock:camera".to_string()))
                );
            }
            _ => panic!("device should be an object"),
        }

        // Check video object with nested resolution
        let video = node_args.get("video").expect("video should exist");
        match video {
            AnyType::Object(video_obj) => {
                assert_eq!(video_obj.len(), 2);
                assert_eq!(video_obj.get("frame_rate"), Some(&AnyType::Int(30)));

                let resolution = video_obj
                    .get("resolution")
                    .expect("resolution should exist");
                match resolution {
                    AnyType::Object(res_obj) => {
                        assert_eq!(res_obj.len(), 2);
                        assert_eq!(res_obj.get("width"), Some(&AnyType::Int(1280)));
                        assert_eq!(res_obj.get("height"), Some(&AnyType::Int(720)));
                    }
                    _ => panic!("resolution should be an object"),
                }
            }
            _ => panic!("video should be an object"),
        }
    }

    #[test]
    fn classify_stage_ready_skips() {
        assert_eq!(
            classify_stage(NodeStage::Ready, "n", "t").expect("Ready should classify"),
            BuildDecision::Skip
        );
    }

    #[test]
    fn classify_stage_added_builds() {
        assert_eq!(
            classify_stage(NodeStage::Added, "n", "t").expect("Added should classify"),
            BuildDecision::Build
        );
    }

    #[test]
    fn classify_stage_building_waits_does_not_rebuild() {
        // Regression: previously stage "Building" fell through to the
        // "build" branch, and the daemon rejected the second build goal
        // with "action already in progress" / "cannot build". The CLI now
        // waits for the in-flight build instead of trying to start a new
        // one.
        assert_eq!(
            classify_stage(NodeStage::Building, "n", "t").expect("Building should classify"),
            BuildDecision::Wait
        );
    }

    #[test]
    fn classify_stage_root_fails_fast() {
        let err = classify_stage(NodeStage::Root, "my_node", "v1")
            .expect_err("Root should fail to classify");
        let msg = format!("{err}");
        assert!(msg.contains("my_node"), "error should name node: {msg}");
        assert!(msg.contains("v1"), "error should name tag: {msg}");
        assert!(
            msg.contains("root"),
            "error should mention root stage: {msg}"
        );
    }

    #[test]
    fn remaining_max_secs_subtracts_elapsed() {
        let remaining = remaining_max_secs(100, 25, "build")
            .expect("remaining budget should still be positive");
        assert_eq!(remaining, 75);
    }

    #[test]
    fn remaining_max_secs_errors_when_budget_exhausted() {
        // Equal to budget: already exhausted (we refuse zero-deadline calls).
        let err_exact =
            remaining_max_secs(30, 30, "run").expect_err("exhausted budget should error");
        let msg = format!("{err_exact}");
        assert!(msg.contains("30s"), "error should cite original max: {msg}");
        assert!(msg.contains("run"), "error should cite stage label: {msg}");
        assert!(
            msg.contains("--max-timeout"),
            "error should hint at the CLI flag: {msg}"
        );

        // Past budget: same error path.
        assert!(remaining_max_secs(30, 45, "run").is_err());
    }

    fn clock_domain(name: &str, core_node: &str, incarnation: u64) -> ClockDomainId {
        ClockDomainId::new(
            Name::new(name).expect("a domain name"),
            CoreNodeName::new(core_node).expect("a machine name"),
            ClockIncarnation::try_from(incarnation).expect("non-zero"),
        )
    }

    fn hosted(name: &str, core_node: &str, incarnation: u64, publisher: &str) -> ClockDomainInfo {
        ClockDomainInfo {
            domain: clock_domain(name, core_node, incarnation),
            publisher_instance_id: publisher.to_owned(),
            launch: None,
            ready: true,
            last_tick_ns: Some(42),
        }
    }

    fn reference(value: &str) -> ClockReference {
        parse_clock_reference(value).expect("the fixture reference should parse")
    }

    /// The `@` splits a machine off the name; a bare name leaves the machine
    /// for the fan-out to settle.
    #[test]
    fn a_clock_reference_splits_its_machine_off_the_name() {
        let bare = reference("robot");
        assert_eq!(bare.name.as_str(), "robot");
        assert_eq!(bare.core_node, None);

        let qualified = reference("robot@cn-sim");
        assert_eq!(qualified.name.as_str(), "robot");
        assert_eq!(
            qualified.core_node.as_ref().map(CoreNodeName::as_str),
            Some("cn-sim")
        );
    }

    #[test]
    fn a_clock_reference_names_which_half_is_malformed() {
        for value in ["", "@cn-sim"] {
            let refusal = parse_clock_reference(value).expect_err("a domain name is never empty");
            assert!(
                refusal.contains("clock domain name"),
                "the refusal should name the domain half: {refusal}"
            );
        }
        let refusal = parse_clock_reference("robot@not a machine")
            .expect_err("a machine name holds no spaces");
        assert!(
            refusal.contains("core node name"),
            "the refusal should name the machine half: {refusal}"
        );
    }

    #[test]
    fn a_published_domain_name_is_checked_where_it_is_typed() {
        assert_eq!(
            parse_clock_domain_name("robot_sim")
                .expect("a plain identifier")
                .as_str(),
            "robot_sim"
        );
        assert!(parse_clock_domain_name("").is_err());
    }

    /// `wall` binds without asking any daemon, and qualifying it by a machine
    /// names nothing one machine supplies to another.
    #[test]
    fn wall_is_settled_without_a_daemon() {
        assert!(refers_to_wall(&reference("wall")).expect("wall is nameable"));
        assert!(!refers_to_wall(&reference("robot")).expect("a domain name is not wall"));

        let refusal = refers_to_wall(&reference("wall@cn-a")).expect_err("wall has no machine");
        let message = refusal.to_string();
        assert!(
            message.contains("--clock wall"),
            "the refusal must say what to type: {message}"
        );
    }

    /// `wall` names the time every machine already keeps, so no command mints
    /// a domain under it: a second row labelled `wall` is one no consumer
    /// could ever bind.
    #[test]
    fn publishing_the_reserved_name_is_refused() {
        let refusal = publisher_binding(&Name::new("wall").expect("a name"), "cn-a")
            .expect_err("`wall` cannot be declared");
        let message = refusal.to_string();
        assert!(
            message.contains("built in") && message.contains("--publish-clock robot_sim"),
            "the refusal must name the reserved word and what to type instead: {message}"
        );
    }

    /// A declaration mints a lifetime of its own, so two runs under one name
    /// are two timelines.
    #[test]
    fn publishing_mints_a_fresh_lifetime_on_the_target_machine() {
        let name = Name::new("robot_sim").expect("a name");
        let first = publisher_binding(&name, "cn-a").expect("a declarable name");
        let second = publisher_binding(&name, "cn-a").expect("a declarable name");

        assert!(first.is_publisher());
        let domain = first.domain().expect("a publisher carries its domain");
        assert_eq!(domain.name.as_str(), "robot_sim");
        assert_eq!(domain.core_node.as_str(), "cn-a");
        assert_ne!(
            domain.incarnation,
            second
                .domain()
                .expect("a publisher carries its domain")
                .incarnation,
            "each declaration mints its own lifetime"
        );
    }

    #[test]
    fn a_short_name_resolves_when_every_daemon_answered() {
        let found = ClockSearch {
            matches: vec![hosted("robot", "cn-a", 7, "sim_1")],
            unreachable: Vec::new(),
        };
        let binding = consumer_binding(&reference("robot"), &found).expect("one match resolves");
        assert_eq!(
            binding.domain().map(ToString::to_string).as_deref(),
            Some("robot@cn-a")
        );
        assert_eq!(
            binding.publisher_ref(),
            Some(&ProducerRef::new("cn-a", "sim_1")),
            "a consumer subscribes to the publisher's own stream"
        );
    }

    #[test]
    fn an_unknown_name_is_refused_with_both_ways_forward() {
        let refusal = consumer_binding(&reference("robot"), &ClockSearch::default())
            .expect_err("an unknown domain is refused");
        let message = refusal.to_string();
        assert!(message.contains("peppy clock list"), "{message}");
        assert!(message.contains("--publish-clock robot"), "{message}");
    }

    /// A daemon that did not answer may be the one hosting the name, so the
    /// refusal reports the gap and never advises minting a second timeline
    /// under a name already in use on another machine.
    #[test]
    fn an_incomplete_answer_is_refused_without_advising_a_declaration() {
        let found = ClockSearch {
            matches: Vec::new(),
            unreachable: vec!["cn-b".to_owned()],
        };
        let refusal = consumer_binding(&reference("robot"), &found)
            .expect_err("an incomplete answer settles nothing");
        let message = refusal.to_string();
        assert!(
            message.contains("cn-b"),
            "the silent peer is named: {message}"
        );
        assert!(
            !message.contains("--publish-clock"),
            "an incomplete answer must not advise declaring the name: {message}"
        );
    }

    /// A short name resolved from a partial answer binds whichever machine
    /// happened to reply, so it is refused with the qualified form to type.
    #[test]
    fn a_short_name_is_refused_while_a_daemon_is_silent() {
        let found = ClockSearch {
            matches: vec![hosted("robot", "cn-a", 7, "sim_1")],
            unreachable: vec!["cn-b".to_owned()],
        };
        let refusal = consumer_binding(&reference("robot"), &found)
            .expect_err("a partial answer leaves a short name in doubt");
        let message = refusal.to_string();
        assert!(message.contains("cn-b"), "{message}");
        assert!(
            message.contains("--clock robot@cn-a"),
            "the refusal must say what to type: {message}"
        );
    }

    /// A reference naming its machine is unambiguous whatever else is
    /// unreachable.
    #[test]
    fn a_qualified_name_resolves_although_a_daemon_is_silent() {
        let found = ClockSearch {
            matches: vec![hosted("robot", "cn-a", 7, "sim_1")],
            unreachable: vec!["cn-b".to_owned()],
        };
        let binding = consumer_binding(&reference("robot@cn-a"), &found)
            .expect("a qualified reference needs no other machine");
        assert_eq!(
            binding.domain().map(ToString::to_string).as_deref(),
            Some("robot@cn-a")
        );
    }

    #[test]
    fn one_name_on_two_machines_is_refused_until_qualified() {
        let found = ClockSearch {
            matches: vec![
                hosted("robot", "cn-a", 7, "sim_1"),
                hosted("robot", "cn-b", 9, "sim_2"),
            ],
            unreachable: Vec::new(),
        };
        let refusal = consumer_binding(&reference("robot"), &found)
            .expect_err("an ambiguous name is refused");
        let message = refusal.to_string();
        assert!(
            message.contains("robot@cn-a") && message.contains("robot@cn-b"),
            "the refusal lists the qualified names to pick from: {message}"
        );
    }
}
