//! Which machines watch each source instance's lifecycle: derived from the
//! plan's observations and from the coordinator, set on this daemon and
//! carried to every participant. The coordinator watches each instance on
//! another machine: the software state of each robot of its fleet reads the
//! start and the end of their processes.

use crate::services::stack::action::StackChangeContext;
use config::runtime::{CoreNodeName, Name};
use daemon_config::launcher::{Placements, PlannedObservation};
use std::collections::{BTreeMap, BTreeSet};

/// The machines watching each source, by source instance.
pub(in crate::services::stack) type LifecycleWatchers = BTreeMap<Name, BTreeSet<CoreNodeName>>;

/// The machines that watch each source: the machine of each remote observer
/// in the complete observation plan, and the coordinator for each instance
/// of `instances` on another machine.
pub(in crate::services::stack) fn lifecycle_watchers<'a>(
    observations: &[PlannedObservation],
    placements: &Placements,
    instances: impl IntoIterator<Item = &'a Name>,
) -> std::result::Result<LifecycleWatchers, String> {
    let mut watchers: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for observation in observations {
        let instance =
            Name::new(&observation.source.instance_id).map_err(|error| error.to_string())?;
        let destinations = watchers.entry(instance).or_default();
        let observer = placements.core_node_of(&observation.observer_instance_id);
        if observer.as_str() != observation.source.core_node {
            destinations.insert(observer.clone());
        }
    }
    let coordinator = placements.coordinator_name();
    for instance in instances {
        if placements.core_node_of(instance.as_str()) != coordinator {
            watchers
                .entry(instance.clone())
                .or_default()
                .insert(coordinator.clone());
        }
    }
    Ok(watchers)
}

/// Points each source this machine hosts at the machines watching it.
pub(in crate::services::stack) fn set_local_watchers(
    ctx: &StackChangeContext,
    watchers: &LifecycleWatchers,
    placements: &Placements,
) {
    for (instance, hosts) in watchers {
        if placements.of(instance.as_str()) == ctx.bound_core_node {
            ctx.relationships.record_watchers(
                instance.as_str(),
                &hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
            );
        }
    }
}

/// The lists that turn `current` into `next`: `next`'s list for every source
/// whose machines differ, an empty list for a source `next` does not watch.
pub(in crate::services::stack) fn watchers_replacing(
    current: &LifecycleWatchers,
    next: &LifecycleWatchers,
) -> LifecycleWatchers {
    let none = BTreeSet::new();
    current
        .keys()
        .chain(next.keys())
        .filter(|source| {
            current.get(*source).unwrap_or(&none) != next.get(*source).unwrap_or(&none)
        })
        .map(|source| {
            (
                source.clone(),
                next.get(source).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::stack::fixtures::placements_with;

    #[test]
    fn lifecycle_watchers_include_existing_and_joined_remote_observers_once() {
        let placements = placements_with(
            "robot",
            &[
                ("alpha_recorder", "cloud"),
                ("bravo_recorder", "cloud"),
                ("charlie_recorder", "edge"),
            ],
        );
        let observation = |observer: &str| PlannedObservation {
            observer_instance_id: observer.into(),
            observer_link_id: "arm".into(),
            pairing_name: "robot_arm".into(),
            pairing_tag: "v1".into(),
            observed_role: "arm".into(),
            source: config::runtime::ProducerRef::new("robot", "arm_inst"),
            source_link_id: "controller".into(),
            peer: None,
        };
        let initial =
            lifecycle_watchers(&[observation("alpha_recorder")], &placements, []).unwrap();
        let joined = lifecycle_watchers(
            &[
                observation("alpha_recorder"),
                observation("bravo_recorder"),
                observation("charlie_recorder"),
                observation("local_recorder"),
            ],
            &placements,
            [],
        )
        .unwrap();
        let source = Name::new("arm_inst").unwrap();
        assert_eq!(
            initial[&source]
                .iter()
                .map(|host| host.as_str())
                .collect::<Vec<_>>(),
            ["cloud"]
        );
        assert_eq!(
            joined[&source]
                .iter()
                .map(|host| host.as_str())
                .collect::<Vec<_>>(),
            ["cloud", "edge"]
        );
        let local = lifecycle_watchers(&[observation("local_recorder")], &placements, []).unwrap();
        assert!(local[&source].is_empty());
    }

    /// The coordinator watches each instance on another machine, beside the
    /// machines of its observers, and no instance on its own machine.
    #[test]
    fn the_coordinator_watches_each_instance_on_another_machine() {
        let placements =
            placements_with("desk", &[("arm_inst", "robot"), ("recorder_inst", "cloud")]);
        let observation = PlannedObservation {
            observer_instance_id: "recorder_inst".into(),
            observer_link_id: "arm".into(),
            pairing_name: "robot_arm".into(),
            pairing_tag: "v1".into(),
            observed_role: "arm".into(),
            source: config::runtime::ProducerRef::new("robot", "arm_inst"),
            source_link_id: "controller".into(),
            peer: None,
        };
        let instances = [
            Name::new("arm_inst").unwrap(),
            Name::new("recorder_inst").unwrap(),
            Name::new("planner_inst").unwrap(),
        ];
        let watchers = lifecycle_watchers(&[observation], &placements, &instances).unwrap();
        let hosts_of = |id: &str| {
            watchers
                .get(&Name::new(id).unwrap())
                .map(|hosts| hosts.iter().map(|host| host.as_str()).collect::<Vec<_>>())
        };
        assert_eq!(hosts_of("arm_inst"), Some(vec!["cloud", "desk"]));
        assert_eq!(hosts_of("recorder_inst"), Some(vec!["desk"]));
        assert_eq!(hosts_of("planner_inst"), None, "it runs on the coordinator");
    }

    /// Every source whose machines change is named: one the next plan
    /// watches from other machines gets that list, one the next plan does not
    /// watch gets an empty list, and one watched the same way by both plans
    /// stays absent.
    #[test]
    fn the_replacing_lists_name_every_source_whose_watchers_change() {
        let source = |id: &str| Name::new(id).unwrap();
        let hosts = |host: &str| BTreeSet::from([CoreNodeName::new(host).unwrap()]);
        let current = LifecycleWatchers::from([
            (source("arm_inst"), hosts("edge")),
            (source("alpha_arm_inst"), hosts("cloud")),
            (source("shared_inst"), hosts("cloud")),
        ]);
        let next = LifecycleWatchers::from([
            (source("arm_inst"), hosts("cloud")),
            (source("shared_inst"), hosts("cloud")),
        ]);

        assert_eq!(
            watchers_replacing(&current, &next),
            LifecycleWatchers::from([
                (source("arm_inst"), hosts("cloud")),
                (source("alpha_arm_inst"), BTreeSet::new()),
            ])
        );
        assert!(watchers_replacing(&next, &next).is_empty());
    }
}
