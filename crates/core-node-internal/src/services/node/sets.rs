//! The set deliveries a join or a removal makes to the instances this daemon
//! runs: producer sets over `binding_update`, observer sets over
//! `observation_update`, through the coordinators that own each.

use super::RelationshipCoordinators;
use config::runtime::{BoundProducers, Name};
use core_node_api::encoding::{ObservationTargets, SlotMembers, SlotSet};
use futures::future::join_all;
use std::collections::BTreeMap;

impl RelationshipCoordinators {
    /// Replaces the set slots of instances this daemon runs, the one path by
    /// which a join's grown sets and a removal's shrunken ones reach running
    /// nodes, whether the coordinator runs the instance or a participant does:
    /// producer sets over `binding_update`, observer sets over
    /// `observation_update`. Every producer set goes out at once, as do the
    /// slots of one observer; observers take the observation coordinator's
    /// lock in turn, which is what keeps a source's generation bump from
    /// interleaving with a delivery, so one call answers within
    /// `OBSERVATION_UPDATE_TIMEOUT` per observer instance plus
    /// `BINDING_UPDATE_TIMEOUT`, the bound `sets_update_budget` sizes a
    /// coordinator's wait by. Returns one line per set that did not reach its
    /// instance, naming the slot and why.
    pub(crate) async fn replace_sets(&self, sets: Vec<SlotSet>) -> Vec<String> {
        let (held, strangers): (Vec<SlotSet>, Vec<SlotSet>) = sets.into_iter().partition(|set| {
            self.node_stack
                .find_by_instance_id(&set.instance_id)
                .is_some()
        });
        let mut failures: Vec<String> = strangers
            .iter()
            .map(|set| {
                let known = self
                    .node_stack
                    .find_entity_label_for_instance_id_any_state(&set.instance_id)
                    .is_some();
                format!(
                    "`{}.links.{}`: {}",
                    set.instance_id,
                    set.link_id,
                    if known {
                        "the instance is not running"
                    } else {
                        "no such instance"
                    }
                )
            })
            .collect();
        // A peer sends whatever it decoded, so one slot named twice is one
        // delivery of the set that came last, for both kinds alike.
        let mut producer_sets: BTreeMap<(Name, String), BoundProducers> = BTreeMap::new();
        let mut observer_sets: BTreeMap<Name, BTreeMap<String, ObservationTargets>> =
            BTreeMap::new();
        for set in held {
            match set.members {
                SlotMembers::Producers(producers) => {
                    producer_sets.insert((set.instance_id, set.link_id), producers);
                }
                SlotMembers::Observed(targets) => {
                    observer_sets
                        .entry(set.instance_id)
                        .or_default()
                        .insert(set.link_id, targets);
                }
            }
        }
        let producers = join_all(producer_sets.into_iter().map(
            |((instance_id, link_id), producers)| async move {
                self.binding
                    .replace_slot(&instance_id, &link_id, producers)
                    .await
                    .err()
                    .map(|reason| format!("`{instance_id}.links.{link_id}`: {reason}"))
            },
        ));
        let observers = join_all(
            observer_sets
                .iter()
                .map(|(observer, slots)| self.observation.replace_slots(observer, slots)),
        );
        let (producers, observers) = futures::join!(producers, observers);
        failures.extend(producers.into_iter().flatten());
        failures.extend(observers.into_iter().flatten());
        failures
    }
}
