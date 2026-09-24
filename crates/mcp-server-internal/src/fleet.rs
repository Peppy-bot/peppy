//! The robots of a per-robot exposure, read from the stack: the members the
//! daemon binds to each target, each with the copy it belongs to, and the
//! resources that follow them as copies join and leave.

use crate::bridges::{self, PreparedResource};
use peppy_mcp_runtime::{FleetHandle, FleetMember, MemberAddress, ResourceIngest};
use peppylib::messaging::ProducerRef;
use peppylib::runtime::{NodeRunner, watch_bound_set};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[cfg(test)]
mod tests;

/// Every holder of an ingest table takes it for a moment and never panics
/// while holding it.
const INGEST_LOCK: &str = "the ingest lock is never poisoned";

/// The members of every target of a per-robot exposure as the stack binds
/// them now: each with the copy it belongs to, as the robot it serves, and
/// the name it has within that copy.
pub(crate) fn members(node_runner: &NodeRunner, targets: &[String]) -> Vec<FleetMember> {
    let processor = node_runner.processor();
    targets
        .iter()
        .flat_map(|target| {
            processor
                .bound_members(target)
                .into_iter()
                .map(|member| FleetMember {
                    target: target.clone(),
                    // A member of a copy is named inside it by the id the
                    // copy's fragment wrote; one outside every copy by the
                    // id it runs as.
                    name: match &member.copy {
                        Some(copy) => copy.instance_id.as_str().to_owned(),
                        None => member.producer.instance_id.clone(),
                    },
                    robot: member.copy.as_ref().map(|copy| copy.name.to_string()),
                    address: MemberAddress {
                        core_node: member.producer.core_node,
                        instance_id: member.producer.instance_id,
                    },
                })
        })
        .collect()
}

/// The ingests of one resource entry, keyed by the producer whose messages
/// fill each. The entry's pump reads it for every message it receives, and
/// the follow loop fills and empties it as the stack binds and drops
/// members.
type EntryIngests = Arc<RwLock<HashMap<ProducerRef, ResourceIngest>>>;

/// The ingests of every resource entry one target publishes, keyed by the
/// entry's catalog name.
type TargetIngests = HashMap<String, EntryIngests>;

/// Starts one pump per resource entry, each following the bound set of its
/// target, and hands back the ingests they deliver through.
fn start_pumps(
    node_runner: &Arc<NodeRunner>,
    resources: Vec<PreparedResource>,
) -> HashMap<String, TargetIngests> {
    let mut by_target: HashMap<String, TargetIngests> = HashMap::new();
    for resource in resources {
        let ingests = EntryIngests::default();
        by_target
            .entry(resource.binding.target.clone())
            .or_default()
            .insert(resource.name.clone(), Arc::clone(&ingests));
        tokio::spawn(bridges::pump_resource(
            Arc::clone(node_runner),
            resource,
            move |producer| ingests.read().expect(INGEST_LOCK).get(producer).cloned(),
        ));
    }
    by_target
}

/// The wire address `member` publishes from.
fn address_of(member: &FleetMember) -> ProducerRef {
    ProducerRef::new(
        member.address.core_node.clone(),
        member.address.instance_id.clone(),
    )
}

/// Registers `member`'s resources and routes each entry's messages to the
/// ingest feeding it. Answers whether the member publishes any resource.
fn attach(
    handle: &FleetHandle,
    ingests: &HashMap<String, TargetIngests>,
    member: &FleetMember,
) -> bool {
    let Some(target) = ingests.get(&member.target) else {
        return false;
    };
    let producer = address_of(member);
    let mut publishes = false;
    for (entry, ingest) in handle.attach(member) {
        if let Some(entry_ingests) = target.get(&entry.name) {
            entry_ingests
                .write()
                .expect(INGEST_LOCK)
                .insert(producer.clone(), ingest);
            publishes = true;
        }
    }
    publishes
}

/// Drops `member`'s resources and the ingests feeding them. Answers whether
/// the member published any resource.
fn detach(
    handle: &FleetHandle,
    ingests: &HashMap<String, TargetIngests>,
    member: &FleetMember,
) -> bool {
    handle.detach(member);
    let Some(target) = ingests.get(&member.target) else {
        return false;
    };
    let producer = address_of(member);
    let mut published = false;
    for entry_ingests in target.values() {
        published |= entry_ingests
            .write()
            .expect(INGEST_LOCK)
            .remove(&producer)
            .is_some();
    }
    published
}

/// Keeps the server's resources in step with the members that run: every
/// member of every target is attached when the stack binds it and detached
/// when the stack drops it, each change to the resource list is announced,
/// and every member the surface cannot serve is logged. The pumps follow
/// the bound producers themselves, so a member joining or leaving adds or
/// drops the ingest its messages fill. Runs until the set watches close.
pub(crate) async fn follow(
    node_runner: Arc<NodeRunner>,
    targets: Vec<String>,
    resources: Vec<PreparedResource>,
    handle: FleetHandle,
) {
    let ingests = start_pumps(&node_runner, resources);
    let (changed, mut changes) = mpsc::channel::<()>(1);
    for target in &targets {
        let mut watch = watch_bound_set(&node_runner, target)
            .expect("the synthesized manifest declares every target of the exposure");
        let changed = changed.clone();
        tokio::spawn(async move {
            while watch.changed().await.is_ok() {
                // A full channel already holds a wake-up for this change.
                let _ = changed.try_send(());
            }
        });
    }
    // The watchers hold the only senders now, so the loop ends with them.
    drop(changed);
    let mut attached: HashSet<FleetMember> = HashSet::new();
    let mut announced = false;
    loop {
        let now = members(&node_runner, &targets);
        let mut touched = false;
        attached.retain(|member| {
            let stays = now.contains(member);
            if !stays {
                touched |= detach(&handle, &ingests, member);
            }
            stays
        });
        for member in now {
            if attached.contains(&member) {
                continue;
            }
            touched |= attach(&handle, &ingests, &member);
            attached.insert(member);
        }
        for problem in handle.problems() {
            tracing::warn!(problem, "a member the surface cannot serve");
        }
        // The first pass builds the list clients read on connect.
        if touched && announced {
            handle.changed();
        }
        announced = true;
        if changes.recv().await.is_none() {
            return;
        }
    }
}
