//! Which machines watch each source instance's lifecycle: derived from the
//! plan's observations, set on this daemon and carried to every participant.

use crate::services::stack::action::StackChangeContext;
use config::runtime::{CoreNodeName, Name};
use daemon_config::launcher::{Placements, PlannedObservation};
use std::collections::{BTreeMap, BTreeSet};

/// The machines watching each source, by source instance.
pub(in crate::services::stack) type LifecycleWatchers = BTreeMap<Name, BTreeSet<CoreNodeName>>;

/// Remote observer destinations for each source in the complete observation plan.
pub(in crate::services::stack) fn lifecycle_watchers(
    observations: &[PlannedObservation],
    placements: &Placements,
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
            ctx.relationships.notifier().set_watchers(
                instance.as_str(),
                &hosts.iter().map(ToString::to_string).collect::<Vec<_>>(),
            );
        }
    }
}

/// The lists that put `previous` back over `established`: the previous plan's
/// watchers, and an empty list for each source the change introduced.
pub(in crate::services::stack) fn watchers_to_restore(
    previous: &LifecycleWatchers,
    established: &LifecycleWatchers,
) -> LifecycleWatchers {
    established
        .keys()
        .map(|source| (source.clone(), BTreeSet::new()))
        .chain(
            previous
                .iter()
                .map(|(source, hosts)| (source.clone(), hosts.clone())),
        )
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
        };
        let initial = lifecycle_watchers(&[observation("alpha_recorder")], &placements).unwrap();
        let joined = lifecycle_watchers(
            &[
                observation("alpha_recorder"),
                observation("bravo_recorder"),
                observation("charlie_recorder"),
                observation("local_recorder"),
            ],
            &placements,
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
        let local = lifecycle_watchers(&[observation("local_recorder")], &placements).unwrap();
        assert!(local[&source].is_empty());
    }

    /// Every source a change re-pointed is named again: one the previous plan
    /// watched gets its own machines back, one the change introduced gets an
    /// empty list, and a source neither plan watched stays absent.
    #[test]
    fn the_restore_lists_undo_every_source_the_change_re_pointed() {
        let source = |id: &str| Name::new(id).unwrap();
        let hosts = |host: &str| BTreeSet::from([CoreNodeName::new(host).unwrap()]);
        let previous = LifecycleWatchers::from([
            (source("arm_inst"), hosts("cloud")),
            (source("shared_inst"), BTreeSet::new()),
        ]);
        let established = LifecycleWatchers::from([
            (source("arm_inst"), hosts("edge")),
            (source("alpha_arm_inst"), hosts("cloud")),
        ]);

        assert_eq!(
            watchers_to_restore(&previous, &established),
            LifecycleWatchers::from([
                (source("arm_inst"), hosts("cloud")),
                (source("alpha_arm_inst"), BTreeSet::new()),
                (source("shared_inst"), BTreeSet::new()),
            ])
        );
        assert!(
            watchers_to_restore(&LifecycleWatchers::new(), &LifecycleWatchers::new()).is_empty()
        );
    }
}
