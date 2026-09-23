//! The robots of a per-robot exposure, read from the stack: the members the
//! daemon binds to each target, each with the copy it belongs to, and the
//! resource pumps that follow them as copies join and leave.

use crate::bridges::{self, PreparedResource};
use peppy_mcp_runtime::{FleetHandle, FleetMember, MemberAddress};
use peppylib::messaging::ProducerRef;
use peppylib::runtime::{NodeRunner, watch_bound_set};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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

/// The pumps feeding one member's resources, dropped when it leaves.
struct Attached {
    pumps: Vec<JoinHandle<()>>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        for pump in &self.pumps {
            pump.abort();
        }
    }
}

/// Keeps the server's resources in step with the members that run: every
/// member of every target is attached with its pumps when the stack binds
/// it and detached when the stack drops it, each change to the resource
/// list is announced, and every member the surface cannot serve is logged.
/// Runs until the set watches close.
pub(crate) async fn follow(
    node_runner: Arc<NodeRunner>,
    targets: Vec<String>,
    resources: HashMap<String, PreparedResource>,
    handle: FleetHandle,
) {
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
    let mut attached: HashMap<FleetMember, Attached> = HashMap::new();
    let mut announced = false;
    loop {
        let now = members(&node_runner, &targets);
        let mut touched = false;
        attached.retain(|member, attachment| {
            let stays = now.contains(member);
            if !stays {
                touched |= !attachment.pumps.is_empty();
                handle.detach(member);
            }
            stays
        });
        for member in now {
            if attached.contains_key(&member) {
                continue;
            }
            let producer = ProducerRef::new(
                member.address.core_node.clone(),
                member.address.instance_id.clone(),
            );
            let pumps: Vec<JoinHandle<()>> = handle
                .attach(&member)
                .into_iter()
                .filter_map(|(entry, ingest)| {
                    let resource = resources.get(&entry.name)?.clone();
                    Some(tokio::spawn(bridges::pump_member_resource(
                        Arc::clone(&node_runner),
                        resource,
                        producer.clone(),
                        ingest,
                    )))
                })
                .collect();
            touched |= !pumps.is_empty();
            attached.insert(member, Attached { pumps });
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
