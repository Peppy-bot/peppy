//! The set deliveries a join or a removal makes to the instances this daemon
//! runs: producer sets over `binding_update`, observer sets over
//! `observation_update`, through the coordinators that own each.

use super::RelationshipCoordinators;
use config::runtime::{BoundProducers, Name};
use core_node_api::InstanceState;
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
    /// coordinator's wait by. A set of a `restarting` instance is recorded on
    /// its slot, which its next process starts from. Returns one line per set
    /// that did not reach its instance, naming the slot and why.
    pub(crate) async fn replace_sets(&self, sets: Vec<SlotSet>) -> Vec<String> {
        let by_state = sets.into_iter().map(|set| {
            let state = self.node_stack.instance_state(set.instance_id.as_str());
            (set, state)
        });
        let (live, others): (Vec<_>, Vec<_>) =
            by_state.partition(|(_, state)| *state == Some(InstanceState::Running));
        let (restarting, not_running): (Vec<_>, Vec<_>) = others
            .into_iter()
            .partition(|(_, state)| *state == Some(InstanceState::Restarting));
        let mut failures = self.record_restarting_sets(restarting.iter().map(|(set, _)| set));
        failures.extend(
            not_running
                .iter()
                .map(|(set, state)| untaken_set_line(set, *state)),
        );
        // A peer sends whatever it decoded, so one slot named twice is one
        // delivery of the set that came last, for both kinds alike.
        let mut producer_sets: BTreeMap<(Name, String), BoundProducers> = BTreeMap::new();
        let mut observer_sets: BTreeMap<Name, BTreeMap<String, ObservationTargets>> =
            BTreeMap::new();
        for (set, _) in live {
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
        // An instance that exits as its set is delivered is `restarting` by
        // the time the delivery fails, unless its exit watcher's in-VM kill of
        // a macOS container outlasts the delivery's timeout; the set is then
        // recorded on its slot like any set of a restarting instance.
        let producers = join_all(producer_sets.into_iter().map(
            |((instance_id, link_id), producers)| async move {
                let Err(reason) = self
                    .binding
                    .replace_slot(&instance_id, &link_id, producers.clone())
                    .await
                else {
                    return None;
                };
                let recorded = self.node_stack.record_slot_binding_for_restart(
                    &instance_id,
                    &link_id,
                    producers,
                );
                (!recorded).then(|| format!("`{instance_id}.links.{link_id}`: {reason}"))
            },
        ));
        let observers = join_all(observer_sets.iter().map(|(observer, slots)| async move {
            let lines = self.observation.replace_slots(observer, slots).await;
            if lines.is_empty() {
                return Vec::new();
            }
            let recorded = self.record_observations_for_restart(observer, slots);
            if recorded { Vec::new() } else { lines }
        }));
        let (producers, observers) = futures::join!(producers, observers);
        failures.extend(producers.into_iter().flatten());
        failures.extend(observers.into_iter().flatten());
        failures
    }

    /// Records the sets of `restarting` instances on their slots, for their
    /// next processes. Returns a line per set whose instance left
    /// `restarting` before it was recorded, naming what it is now.
    fn record_restarting_sets<'a>(&self, sets: impl Iterator<Item = &'a SlotSet>) -> Vec<String> {
        sets.filter(|set| !self.record_for_restart(set))
            .map(|set| {
                let state = self.node_stack.instance_state(set.instance_id.as_str());
                untaken_set_line(set, state)
            })
            .collect()
    }

    /// Records a set on the slot of a `restarting` instance, for its next
    /// process. `false` when the instance is no longer `restarting`.
    fn record_for_restart(&self, set: &SlotSet) -> bool {
        match &set.members {
            SlotMembers::Producers(producers) => self.node_stack.record_slot_binding_for_restart(
                &set.instance_id,
                &set.link_id,
                producers.clone(),
            ),
            SlotMembers::Observed(targets) => self.record_observations_for_restart(
                &set.instance_id,
                &BTreeMap::from([(set.link_id.clone(), targets.clone())]),
            ),
        }
    }

    /// Records observer sets on the slot of a `restarting` instance. `false`
    /// when the instance is not `restarting`.
    fn record_observations_for_restart(
        &self,
        observer: &Name,
        slots: &BTreeMap<String, ObservationTargets>,
    ) -> bool {
        self.node_stack
            .record_observations_for_restart(observer.as_str(), slots)
    }
}

/// The line for a set this daemon did not take: its instance is in `state`,
/// or the stack does not track it (`None`).
fn untaken_set_line(set: &SlotSet, state: Option<InstanceState>) -> String {
    let why = match state {
        Some(state) => format!("the instance is {state}"),
        None => "no such instance".to_owned(),
    };
    format!("`{}.links.{}`: {why}", set.instance_id, set.link_id)
}
