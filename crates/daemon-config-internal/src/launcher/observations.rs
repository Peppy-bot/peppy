//! Plan-phase validation for the observer-slot entries of the launcher's
//! per-instance `links` map and its `defer_links` field (and the CLI's
//! `--link` / `--defer-link`, which feed the same validator through the
//! daemon).
//!
//! An observer slot (`depends_on.pairings` with `observes_role`) passively taps
//! the topics a participant emits for that role, without joining the 1:1
//! pairing and without claiming any endpoint. So, unlike pairing, observation
//! is neither exclusive nor two-sided: many observers may watch the same
//! source, and the source is never coverage-checked for the observer. Every
//! observer slot is required (the manifest forbids `optional` on the observer
//! form), so each must be linked to a source or explicitly deferred, or the
//! plan is rejected (`ObservationSlotUncovered`).

use crate::error::{
    ObservationSlotUncovered, ObservationTargetAmbiguous, ObservationTargetNotObservable,
    PairingSha256Mismatch, ParsingError,
};
use config::node::{PairingDependency, PairingObserverDependency, PairingParticipantDependency};
use config::runtime::ProducerRef;
use std::collections::BTreeMap;

use super::pairings::PairingValidationItem;
use super::types::split_pair_target;

/// The observer slots of a pairing-dep list, in declaration order. Participant
/// slots are handled by `pairings`; observation validation steps over them.
fn observers(deps: &[PairingDependency]) -> impl Iterator<Item = &PairingObserverDependency> {
    deps.iter().filter_map(|dep| match dep {
        PairingDependency::Observer(observer) => Some(observer),
        PairingDependency::Participant(_) => None,
    })
}

/// The participant slots of a pairing-dep list. Duplicated from `pairings`
/// (private there) because observation resolves an observer's source against
/// the source instance's participant slots.
fn participants(deps: &[PairingDependency]) -> impl Iterator<Item = &PairingParticipantDependency> {
    deps.iter().filter_map(|dep| match dep {
        PairingDependency::Participant(participant) => Some(participant),
        PairingDependency::Observer(_) => None,
    })
}

/// One validated observation, ready for the daemon to deliver to the observer
/// once resolved. The observer subscribes fully pinned to
/// `(source.core_node, source.instance_id, source_link_id)` and follows that
/// source instance's own lifecycle, not any peer relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedObservation {
    pub observer_instance_id: String,
    /// The observer's own slot `link_id` (local resolution name).
    pub observer_link_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub observed_role: String,
    /// The observed source instance, stamped with the launching daemon's
    /// `core_node` exactly as a resolved binding is.
    pub source: ProducerRef,
    /// The producer-side `link_id` of the source's participant slot: the
    /// segment the source publishes its role topics under, and the third
    /// element of the observer's subscription pin.
    pub source_link_id: String,
}

/// Outcome of [`validate_observations`]: aggregated rule violations plus the
/// resolved observation plan. The caller must check `errors.is_empty()` before
/// consuming `planned`.
#[derive(Debug, Default)]
pub struct ValidatedObservations {
    pub errors: Vec<ParsingError>,
    pub planned: Vec<PlannedObservation>,
}

/// Run all observation validator rules over the plan. `producer_core_node` is
/// the launching daemon's core_node, stamped into every resolved source
/// [`ProducerRef`] exactly as [`super::validate_bindings`] stamps bindings.
///
/// Rules:
/// 1. Only `links` keys naming one of this node's observer slots are processed;
///    an observer entry whose value is an array is `LinkTargetNotScalar`
///    (raised by `pairings`' scalar guard is not shared, so it is raised here).
/// 2. The source instance exists in the plan/stack (`UnknownInstanceId`).
/// 3. The source declares exactly one participant slot playing `observes_role`
///    for the observer's pairing `(name, tag)`, or the link names one via the
///    `/<source_link_id>` suffix (`ObservationTargetNotObservable` /
///    `ObservationTargetAmbiguous`). Observation is not exclusive, so every
///    such slot is a candidate regardless of who else observes it.
/// 4. When both the observer slot and the resolved source slot pin a `sha256`,
///    the pins must match (`PairingSha256Mismatch`).
/// 5. Coverage: every observer slot of every planned instance is linked or
///    listed in `defer_links` (`ObservationSlotUncovered`).
pub fn validate_observations(
    items: &[PairingValidationItem<'_>],
    producer_core_node: &str,
) -> ValidatedObservations {
    let mut out = ValidatedObservations::default();

    // instance_id → owning item (any state, so a preexisting source resolves).
    let mut lookup: BTreeMap<&str, &PairingValidationItem<'_>> = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            lookup.entry(instance.instance_id.as_str()).or_insert(item);
        }
    }

    for item in items.iter().filter(|i| !i.preexisting) {
        let observer_link_ids: std::collections::BTreeSet<&str> =
            observers(item.pairing_deps).map(|o| o.link_id.as_str()).collect();

        for instance in item.instances {
            let owner_id = instance.instance_id.as_str();

            for (key, value) in &instance.links {
                if !observer_link_ids.contains(key.as_str()) {
                    continue;
                }
                let Some(target) = value.as_scalar() else {
                    out.errors.push(ParsingError::LinkTargetNotScalar {
                        owner_instance_id: owner_id.to_string(),
                        link: key.clone(),
                    });
                    continue;
                };
                match resolve_observation(owner_id, item, key, target, &lookup, producer_core_node) {
                    Ok(planned) => out.planned.push(planned),
                    Err(error) => out.errors.push(error),
                }
            }

            validate_coverage(instance, item, &mut out.errors);
        }
    }

    out
}

/// Resolves ONE observer `links` entry (rules 2-4). `own_dep` is guaranteed to
/// be an observer slot of `item` because the caller filters on that.
fn resolve_observation(
    owner_id: &str,
    item: &PairingValidationItem<'_>,
    key: &str,
    target: &str,
    lookup: &BTreeMap<&str, &PairingValidationItem<'_>>,
    producer_core_node: &str,
) -> Result<PlannedObservation, ParsingError> {
    let own_dep = observers(item.pairing_deps)
        .find(|o| o.link_id == key)
        .expect("link key is an observer slot of this item");

    let (source_instance, requested_source_link) = split_pair_target(target);
    let Some(source_item) = lookup.get(source_instance) else {
        return Err(ParsingError::UnknownInstanceId {
            owner_instance_id: owner_id.to_string(),
            link: key.to_string(),
            instance_id: source_instance.to_string(),
        });
    };

    // Candidate source slots: participant slots on the source instance playing
    // the observed role for the observer's pairing (name, tag). Observation is
    // not exclusive, so no claim filtering: every match is a candidate.
    let candidates: Vec<&PairingParticipantDependency> = participants(source_item.pairing_deps)
        .filter(|p| {
            p.name == own_dep.name && p.tag == own_dep.tag && p.role == own_dep.observes_role
        })
        .collect();

    let not_observable = || {
        ParsingError::ObservationTargetNotObservable(Box::new(ObservationTargetNotObservable {
            owner_instance_id: owner_id.to_string(),
            key: key.to_string(),
            source_instance_id: source_instance.to_string(),
            source_name: source_item.node_name.to_string(),
            source_tag: source_item.node_tag.to_string(),
            pairing_name: own_dep.name.as_str().to_string(),
            pairing_tag: own_dep.tag.clone(),
            observed_role: own_dep.observes_role.clone(),
        }))
    };

    let source_dep = if let Some(requested) = requested_source_link {
        match candidates.iter().find(|p| p.link_id == requested) {
            Some(dep) => *dep,
            None => return Err(not_observable()),
        }
    } else {
        match candidates.as_slice() {
            [] => return Err(not_observable()),
            [single] => single,
            multiple => {
                let candidate_link_ids = multiple
                    .iter()
                    .map(|p| p.link_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ParsingError::ObservationTargetAmbiguous(Box::new(
                    ObservationTargetAmbiguous {
                        owner_instance_id: owner_id.to_string(),
                        key: key.to_string(),
                        source_instance_id: source_instance.to_string(),
                        pairing_name: own_dep.name.as_str().to_string(),
                        pairing_tag: own_dep.tag.clone(),
                        observed_role: own_dep.observes_role.clone(),
                        candidate_link_ids,
                    },
                )));
            }
        }
    };

    // Rule 4: both-pinned sha256 must match.
    if let (Some(sha_own), Some(sha_source)) = (own_dep.sha256.as_deref(), source_dep.sha256.as_deref())
        && sha_own != sha_source
    {
        return Err(ParsingError::PairingSha256Mismatch(Box::new(
            PairingSha256Mismatch {
                instance_a: owner_id.to_string(),
                sha_a: sha_own.to_string(),
                instance_b: source_instance.to_string(),
                sha_b: sha_source.to_string(),
                pairing_name: own_dep.name.as_str().to_string(),
                pairing_tag: own_dep.tag.clone(),
            },
        )));
    }

    Ok(PlannedObservation {
        observer_instance_id: owner_id.to_string(),
        observer_link_id: key.to_string(),
        pairing_name: own_dep.name.as_str().to_string(),
        pairing_tag: own_dep.tag.clone(),
        observed_role: own_dep.observes_role.clone(),
        source: ProducerRef::new(producer_core_node, source_instance),
        source_link_id: source_dep.link_id.clone(),
    })
}

/// Rule 5: every observer slot must be linked (a `links` entry) or deferred (a
/// `defer_links` entry). Observer slots are always required.
fn validate_coverage(
    instance: &super::types::DeploymentInstance,
    item: &PairingValidationItem<'_>,
    errors: &mut Vec<ParsingError>,
) {
    let owner_id = instance.instance_id.as_str();
    for observer in observers(item.pairing_deps) {
        let covered = instance.links.contains_key(&observer.link_id)
            || instance.defer_links.contains(&observer.link_id);
        if !covered {
            errors.push(ParsingError::ObservationSlotUncovered(Box::new(
                ObservationSlotUncovered {
                    instance_id: owner_id.to_string(),
                    link_id: observer.link_id.clone(),
                    pairing_name: observer.name.as_str().to_string(),
                    pairing_tag: observer.tag.clone(),
                    observed_role: observer.observes_role.clone(),
                },
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::DeploymentInstance;

    const TEST_CORE: &str = "core_a";

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_pairing_deps(json5: &str) -> Vec<PairingDependency> {
        serde_json5::from_str(json5).expect("pairing deps fixture should parse")
    }

    /// A robot arm exposing the `arm` participant role of `arm_link/v1`.
    fn arm_deps() -> Vec<PairingDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true }]"#,
        )
    }

    /// A recorder observing the `arm` role of `arm_link/v1` through slot
    /// `observed_arm`.
    fn recorder_deps() -> Vec<PairingDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", observes_role: "arm", link_id: "observed_arm" }]"#,
        )
    }

    fn item<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        pairing_deps: &'a [PairingDependency],
    ) -> PairingValidationItem<'a> {
        PairingValidationItem {
            node_name,
            node_tag: "v1",
            instances,
            pairing_deps,
            preexisting: false,
        }
    }

    #[test]
    fn observer_resolves_against_the_participant_source() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: "arm_1" } }]"#,
        );
        let rec_deps = recorder_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
        let obs = &out.planned[0];
        assert_eq!(obs.observer_instance_id, "rec_1");
        assert_eq!(obs.observer_link_id, "observed_arm");
        assert_eq!(obs.observed_role, "arm");
        assert_eq!(obs.source, ProducerRef::new(TEST_CORE, "arm_1"));
        // The source publishes the `arm` role under its own participant slot.
        assert_eq!(obs.source_link_id, "controller");
    }

    #[test]
    fn observer_without_source_is_uncovered() {
        let rec_instances = parse_instances(r#"[{ instance_id: "rec_1" }]"#);
        let rec_deps = recorder_deps();
        let items = vec![item("recorder", &rec_instances, &rec_deps)];
        let out = validate_observations(&items, TEST_CORE);
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::ObservationSlotUncovered(info) => Some(info),
                _ => None,
            })
            .expect("expected ObservationSlotUncovered");
        assert_eq!(info.instance_id, "rec_1");
        assert_eq!(info.link_id, "observed_arm");
        assert_eq!(info.observed_role, "arm");
        assert!(
            info.to_string().contains("--defer-link observed_arm"),
            "message should show the defer: {info}"
        );
    }

    #[test]
    fn deferred_observer_is_covered() {
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", defer_links: ["observed_arm"] }]"#);
        let rec_deps = recorder_deps();
        let items = vec![item("recorder", &rec_instances, &rec_deps)];
        let out = validate_observations(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());
    }

    #[test]
    fn source_not_playing_the_observed_role_is_rejected() {
        // The source only plays `controller`, so nothing emits the `arm` role.
        let ctrl_instances = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let ctrl_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }]"#,
        );
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: "ctrl_1" } }]"#);
        let rec_deps = recorder_deps();
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::ObservationTargetNotObservable(info) => Some(info),
                _ => None,
            })
            .expect("expected ObservationTargetNotObservable");
        assert_eq!(info.source_instance_id, "ctrl_1");
        assert_eq!(info.observed_role, "arm");
    }

    #[test]
    fn ambiguous_source_requires_disambiguation() {
        // A dual-arm source plays `arm` through two participant slots.
        let dual_instances = parse_instances(r#"[{ instance_id: "dual_1" }]"#);
        let dual_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl", optional: true },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl", optional: true }
            ]"#,
        );
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: "dual_1" } }]"#);
        let rec_deps = recorder_deps();
        let items = vec![
            item("dual_arm", &dual_instances, &dual_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::ObservationTargetAmbiguous(info) => Some(info),
                _ => None,
            })
            .expect("expected ObservationTargetAmbiguous");
        assert_eq!(info.candidate_link_ids, "left_ctl, right_ctl");

        // The `/<source_link_id>` suffix resolves it and pins that slot.
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: "dual_1/right_ctl" } }]"#,
        );
        let items = vec![
            item("dual_arm", &dual_instances, &dual_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned[0].source_link_id, "right_ctl");
    }

    #[test]
    fn many_observers_may_watch_one_source() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[
                { instance_id: "rec_1", links: { observed_arm: "arm_1" } },
                { instance_id: "rec_2", links: { observed_arm: "arm_1" } }
            ]"#,
        );
        let rec_deps = recorder_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 2, "observation is not exclusive");
    }

    #[test]
    fn observer_value_as_array_is_rejected() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_1"] } }]"#,
        );
        let rec_deps = recorder_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, TEST_CORE);
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::LinkTargetNotScalar { .. })),
            "an array value on an observer slot is rejected: {:?}",
            out.errors
        );
    }
}
