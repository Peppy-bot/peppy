//! Plan-phase validation for the observer-slot entries of the launcher's
//! per-instance `links` map (and the CLI's `--link` / `--vacant-link`, which
//! feed the same validator through the daemon).
//!
//! An observer slot (`depends_on.pairing_observers`) passively taps
//! the topics a participant emits for that role, without joining the 1:1
//! pairing and without claiming any endpoint. So, unlike pairing, observation
//! is neither exclusive nor two-sided: many observers may watch the same
//! source, and the source is never coverage-checked for the observer.
//!
//! A slot observes as many pairings as its declared `cardinality` allows, and
//! its link value's shape follows that cardinality exactly as a producer
//! binding's does: a scalar slot (`one` or `zero_or_one`) takes a single
//! target, the multi cardinalities take an array. Every slot but a
//! `zero_or_more` one must carry a `links` entry (`ObservationSlotUncovered`
//! otherwise), and a `zero_or_one` slot, the one the node's manifest declares
//! emptiable, may spend that entry on `{ vacant: "<why>" }`; omitting a
//! `zero_or_more` slot IS its empty set.

use super::types::Placements;
use crate::error::{
    ObservationSlotUncovered, ObservationTargetAmbiguous, ObservationTargetNotObservable,
    PairingSha256Mismatch, ParsingError,
};
use config::node::PairingObserverDependency;
use config::runtime::ProducerRef;
use std::collections::{BTreeMap, HashSet};

use super::pairings::PairingValidationItem;
use super::types::{
    CardinalityShapeViolation, Selection, check_cardinality_shape, split_link_target,
};

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
///    the value's shape must match the slot's declared cardinality
///    (`ObservationArrayOnScalarSlot` / `ObservationScalarOnMultiSlot` /
///    `ObservationCardinalityUnmet` / `ObservationSingleSlotMultipleTargets`).
/// 2. The source instance exists in the plan/stack (`UnknownInstanceId`).
/// 3. The source declares exactly one participant slot playing the observed
///    `role` for the observer's pairing `(name, tag)`, or the link names one
///    via the
///    `/<source_link_id>` suffix (`ObservationTargetNotObservable` /
///    `ObservationTargetAmbiguous`). Observation is not exclusive, so every
///    such slot is a candidate regardless of who else observes it.
/// 4. When both the observer slot and the resolved source slot pin a `sha256`,
///    the pins must match (`PairingSha256Mismatch`).
/// 5. No two of a slot's targets resolve to the same observed pairing
///    (`DuplicateObservationTarget`).
/// 6. Coverage: every observer slot but a `zero_or_more` one, on every planned
///    instance, carries a `links` entry, naming sources or (on a `zero_or_one`
///    slot) declaring it vacant (`ObservationSlotUncovered` otherwise).
///
/// Rules 2-5 are all-or-nothing per slot: a slot with any failing member
/// contributes no observation at all, so a partially-resolved member set can
/// never reach the plan.
pub fn validate_observations(
    items: &[PairingValidationItem<'_>],
    placements: &Placements,
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
        let observers_by_link: BTreeMap<&str, &PairingObserverDependency> = item
            .observer_deps
            .iter()
            .map(|observer| (observer.link_id.as_str(), observer))
            .collect();

        for instance in item.instances {
            let owner_id = instance.instance_id.as_str();

            for (key, value) in &instance.links {
                let Some(own_dep) = observers_by_link.get(key.as_str()).copied() else {
                    continue;
                };
                // A vacant slot observes nothing on purpose: it covers the
                // slot (rule 6) and resolves no members.
                let Some(selection) = value.selection() else {
                    continue;
                };
                match resolve_observation_slot(
                    owner_id, own_dep, key, selection, &lookup, placements,
                ) {
                    Ok(planned) => out.planned.extend(planned),
                    Err(errors) => out.errors.extend(errors),
                }
            }

            validate_coverage(instance, item, &mut out.errors);
        }
    }

    out
}

/// Resolves ONE observer slot's whole selection: the shape check (rule 1),
/// then every target in declaration order (rules 2-4), then the duplicate-member
/// check (rule 5). Returns the slot's member set in that same order, or every
/// rule violation it collected; a slot that fails contributes nothing.
fn resolve_observation_slot(
    owner_id: &str,
    own_dep: &PairingObserverDependency,
    key: &str,
    selection: &Selection,
    lookup: &BTreeMap<&str, &PairingValidationItem<'_>>,
    placements: &Placements,
) -> Result<Vec<PlannedObservation>, Vec<ParsingError>> {
    if let Err(violation) = check_cardinality_shape(own_dep.cardinality, selection) {
        return Err(vec![shape_error(violation, owner_id, key)]);
    }

    let mut planned = Vec::with_capacity(selection.targets().len());
    let mut errors = Vec::new();
    for target in selection.targets() {
        match resolve_observation(owner_id, own_dep, key, target, lookup, placements) {
            Ok(observation) => planned.push(observation),
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Distinct target strings can resolve to one pairing: on a source with a
    // single observable slot, `dual_1` and `dual_1/left_ctl` are the same
    // member. The raw strings were deduplicated at parse; this is the check
    // that only resolution can make.
    let mut seen = HashSet::with_capacity(planned.len());
    if let Some(duplicate) = planned
        .iter()
        .find(|observation| !seen.insert((&observation.source, &observation.source_link_id)))
    {
        return Err(vec![ParsingError::DuplicateObservationTarget {
            owner_instance_id: owner_id.to_string(),
            link: key.to_string(),
            source_instance_id: duplicate.source.instance_id.clone(),
            source_link_id: duplicate.source_link_id.clone(),
        }]);
    }

    Ok(planned)
}

/// Speaks a [`CardinalityShapeViolation`] in observer vocabulary, the way
/// `bindings` speaks it in producer-binding vocabulary.
fn shape_error(violation: CardinalityShapeViolation, owner_id: &str, key: &str) -> ParsingError {
    let owner_instance_id = owner_id.to_string();
    let link = key.to_string();
    match violation {
        CardinalityShapeViolation::ArrayOnScalarSlot { cardinality } => {
            ParsingError::ObservationArrayOnScalarSlot {
                owner_instance_id,
                link,
                cardinality,
            }
        }
        CardinalityShapeViolation::ScalarOnMultiSlot { cardinality } => {
            ParsingError::ObservationScalarOnMultiSlot {
                owner_instance_id,
                link,
                cardinality,
            }
        }
        CardinalityShapeViolation::Unmet => ParsingError::ObservationCardinalityUnmet {
            owner_instance_id,
            link,
        },
        CardinalityShapeViolation::SingleSlotMultipleTargets {
            cardinality,
            target_count,
        } => ParsingError::ObservationSingleSlotMultipleTargets {
            owner_instance_id,
            link,
            cardinality,
            target_count,
        },
    }
}

/// Resolves ONE observer `links` entry (rules 2-4). `own_dep` is guaranteed to
/// be an observer slot of `item` because the caller filters on that.
fn resolve_observation(
    owner_id: &str,
    own_dep: &PairingObserverDependency,
    key: &str,
    target: &str,
    lookup: &BTreeMap<&str, &PairingValidationItem<'_>>,
    placements: &Placements,
) -> Result<PlannedObservation, ParsingError> {
    let (source_instance, requested_source_link) = split_link_target(target);
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
    let candidates: Vec<_> = source_item
        .pairing_deps
        .iter()
        .filter(|p| p.name == own_dep.name && p.tag == own_dep.tag && p.role == own_dep.role)
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
            observed_role: own_dep.role.clone(),
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
                        observed_role: own_dep.role.clone(),
                        candidate_link_ids,
                    },
                )));
            }
        }
    };

    // Rule 4: both-pinned sha256 must match.
    if let (Some(sha_own), Some(sha_source)) =
        (own_dep.sha256.as_deref(), source_dep.sha256.as_deref())
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
        observed_role: own_dep.role.clone(),
        source: ProducerRef::new(placements.of(source_instance), source_instance),
        source_link_id: source_dep.link_id.clone(),
    })
}

/// Rule 6: every observer slot but a `zero_or_more` one must carry a `links`
/// entry, whether that entry names sources or declares the slot vacant.
/// Omitting a `zero_or_more` slot is how a deployment writes its empty set, so
/// it needs no coverage; every other slot needs an explicit line, which is what
/// keeps "forgot" distinguishable from "decided".
///
/// A vacancy covers a `zero_or_one` slot ONLY, because that is the one observer
/// cardinality the node's manifest declares emptiable. On any other slot a
/// vacancy covers nothing here and is separately rejected by
/// `validate_link_slots` with the reason; judging it independently is what
/// keeps the generated "the plan bound at least one pairing to it" docstring
/// true no matter which validator runs first.
fn validate_coverage(
    instance: &super::types::DeploymentInstance,
    item: &PairingValidationItem<'_>,
    errors: &mut Vec<ParsingError>,
) {
    let owner_id = instance.instance_id.as_str();
    for observer in item.observer_deps {
        let covered = match instance.links.get(&observer.link_id) {
            Some(value) => {
                value.selection().is_some()
                    || observer.cardinality == config::node::Cardinality::ZeroOrOne
            }
            None => false,
        };
        if covered || observer.cardinality.allows_empty() {
            continue;
        }
        errors.push(ParsingError::ObservationSlotUncovered(Box::new(
            ObservationSlotUncovered {
                instance_id: owner_id.to_string(),
                link_id: observer.link_id.clone(),
                pairing_name: observer.name.as_str().to_string(),
                pairing_tag: observer.tag.clone(),
                observed_role: observer.role.clone(),
                cardinality: observer.cardinality,
            },
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::DeploymentInstance;
    use super::*;

    const TEST_CORE: &str = "core_a";

    /// Every test instance on one daemon, the single-machine shape.
    fn all_local() -> Placements {
        Placements::all_on(
            crate::core_node_name::CoreNodeName::new(TEST_CORE).expect("valid test core node name"),
        )
    }

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_pairing_deps(json5: &str) -> Vec<config::node::PairingParticipantDependency> {
        serde_json5::from_str(json5).expect("pairing deps fixture should parse")
    }

    /// A robot arm exposing the `arm` participant role of `arm_link/v1`.
    fn arm_deps() -> Vec<config::node::PairingParticipantDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]"#,
        )
    }

    fn parse_observer_deps(json5: &str) -> Vec<PairingObserverDependency> {
        serde_json5::from_str(json5).expect("observer deps fixture should parse")
    }

    /// A recorder observing the `arm` role of `arm_link/v1` through slot
    /// `observed_arm`, at the given cardinality (omitted spelling `one`).
    fn recorder_deps_with(cardinality: Option<&str>) -> Vec<PairingObserverDependency> {
        let cardinality = match cardinality {
            Some(spelling) => format!(r#", cardinality: "{spelling}""#),
            None => String::new(),
        };
        parse_observer_deps(&format!(
            r#"[{{ name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm"{cardinality} }}]"#
        ))
    }

    /// The default `one` recorder, the shape most rules are stated on.
    fn recorder_deps() -> Vec<PairingObserverDependency> {
        recorder_deps_with(None)
    }

    fn planned_sources(out: &ValidatedObservations) -> Vec<(&str, &str)> {
        out.planned
            .iter()
            .map(|observation| {
                (
                    observation.source.instance_id.as_str(),
                    observation.source_link_id.as_str(),
                )
            })
            .collect()
    }

    /// A node declaring participant slots only (a potential observation source).
    fn item<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        pairing_deps: &'a [config::node::PairingParticipantDependency],
    ) -> PairingValidationItem<'a> {
        PairingValidationItem {
            node_name,
            node_tag: "v1",
            instances,
            pairing_deps,
            observer_deps: &[],
            preexisting: false,
        }
    }

    /// A node declaring observer slots only.
    fn observer_item<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        observer_deps: &'a [PairingObserverDependency],
    ) -> PairingValidationItem<'a> {
        PairingValidationItem {
            observer_deps,
            pairing_deps: &[],
            ..item(node_name, instances, &[])
        }
    }

    #[test]
    fn observer_resolves_against_the_participant_source() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: "arm_1" } }]"#);
        let rec_deps = recorder_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
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

    /// A `one` slot must observe exactly one pairing, so its uncovered message
    /// offers a source and the manifest key that would let it observe none, and
    /// never the vacancy spelling its manifest has not enabled.
    #[test]
    fn observer_without_source_is_uncovered() {
        let rec_instances = parse_instances(r#"[{ instance_id: "rec_1" }]"#);
        let rec_deps = recorder_deps();
        let items = vec![observer_item("recorder", &rec_instances, &rec_deps)];
        let out = validate_observations(&items, &all_local());
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
        let message = info.to_string();
        assert!(
            message.contains("cardinality: \"zero_or_one\""),
            "message should name the manifest key that lets it observe nothing: {message}"
        );
        assert!(
            message.contains("if it is meant to run empty, declare"),
            "the vacancy must be offered only behind the manifest change, never as a \
             standalone fix: {message}"
        );
    }

    /// A `zero_or_one` slot is the one observer cardinality a node declares
    /// emptiable, so a vacancy covers it and the uncovered message offers the
    /// spelling.
    #[test]
    fn a_vacant_zero_or_one_observer_is_covered() {
        let rec_deps = recorder_deps_with(Some("zero_or_one"));

        let vacant = parse_instances(
            r#"[{
                instance_id: "rec_1",
                links: { observed_arm: { vacant: "bench rig: no arm to watch" } }
            }]"#,
        );
        let out = validate_observations(
            &[observer_item("recorder", &vacant, &rec_deps)],
            &all_local(),
        );
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());

        // Omitting the slot is still uncovered: only a written vacancy empties
        // it, which is what keeps "forgot" distinguishable from "decided".
        let omitted = parse_instances(r#"[{ instance_id: "rec_1" }]"#);
        let out = validate_observations(
            &[observer_item("recorder", &omitted, &rec_deps)],
            &all_local(),
        );
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::ObservationSlotUncovered(info) => Some(info),
                _ => None,
            })
            .expect("an omitted zero_or_one slot is uncovered");
        assert!(
            info.to_string()
                .contains("--vacant-link 'observed_arm=<why>'"),
            "message should show how to declare it vacant: {info}"
        );
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
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
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
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl" }
            ]"#,
        );
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: "dual_1" } }]"#);
        let rec_deps = recorder_deps();
        let items = vec![
            item("dual_arm", &dual_instances, &dual_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
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
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
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
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 2, "observation is not exclusive");
    }

    /// An observer link's shape mirrors the slot's declared cardinality
    /// exactly as a producer binding's does, and the rejections are named in
    /// observer vocabulary.
    #[test]
    fn observer_link_shape_must_match_the_slot_cardinality() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();

        // An array on a `one` slot, single-element included.
        let one_deps = recorder_deps_with(None);
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_1"] } }]"#);
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &one_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::ObservationArrayOnScalarSlot { .. })),
            "an array on a `one` observer slot is rejected: {:?}",
            out.errors
        );

        // A scalar on a multi slot.
        for cardinality in ["one_or_more", "zero_or_more"] {
            let multi_deps = recorder_deps_with(Some(cardinality));
            let rec_instances =
                parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: "arm_1" } }]"#);
            let items = vec![
                item("robot_arm", &arm_instances, &arm_deps),
                observer_item("recorder", &rec_instances, &multi_deps),
            ];
            let out = validate_observations(&items, &all_local());
            assert!(
                out.errors
                    .iter()
                    .any(|e| matches!(e, ParsingError::ObservationScalarOnMultiSlot { .. })),
                "a scalar on a `{cardinality}` observer slot is rejected: {:?}",
                out.errors
            );
        }

        // An empty array on `one_or_more` (valid only on `zero_or_more`).
        let one_or_more_deps = recorder_deps_with(Some("one_or_more"));
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: [] } }]"#);
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &one_or_more_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::ObservationCardinalityUnmet { .. })),
            "an empty array on `one_or_more` is rejected: {:?}",
            out.errors
        );

        let zero_or_more_deps = recorder_deps_with(Some("zero_or_more"));
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &zero_or_more_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors.is_empty(),
            "an empty array on `zero_or_more` is its empty set: {:?}",
            out.errors
        );
        assert!(out.planned.is_empty());
    }

    /// Repeated `--link` occurrences accumulate a multi slot's member set and
    /// stay a hard error on a `one` slot.
    #[test]
    fn repeated_link_flags_are_counted_against_the_cardinality() {
        let arm_instances =
            parse_instances(r#"[{ instance_id: "arm_1" }, { instance_id: "arm_2" }]"#);
        let arm_deps = arm_deps();
        // `--link` occurrences have no launch-file spelling, so the flag value
        // is built the way the CLI builds it.
        let flags = |targets: &[&str]| {
            let mut instances = parse_instances(r#"[{ instance_id: "rec_1" }]"#);
            instances[0].links.insert(
                "observed_arm".to_string(),
                super::super::types::LinkValue::Bound(Selection::Flags(
                    super::super::types::LinkTargets::new(
                        targets.iter().map(|target| target.to_string()).collect(),
                    )
                    .expect("distinct flag targets"),
                )),
            );
            instances
        };

        let one_deps = recorder_deps_with(None);
        let rec_instances = flags(&["arm_1", "arm_2"]);
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &one_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors.iter().any(|e| matches!(
                e,
                ParsingError::ObservationSingleSlotMultipleTargets {
                    target_count: 2,
                    ..
                }
            )),
            "two `--link` occurrences on a `one` slot are rejected: {:?}",
            out.errors
        );

        let multi_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &multi_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            planned_sources(&out),
            [("arm_1", "controller"), ("arm_2", "controller")],
            "flag order is the slot's member order"
        );
    }

    /// A multi slot resolves every target, in the order the launcher wrote
    /// them: that order reaches the node, so a deployment can pair member N
    /// with its own Nth command slot.
    #[test]
    fn a_multi_slot_resolves_every_target_in_declaration_order() {
        let arm_instances =
            parse_instances(r#"[{ instance_id: "arm_1" }, { instance_id: "arm_2" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_2", "arm_1"] } }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            planned_sources(&out),
            [("arm_2", "controller"), ("arm_1", "controller")],
            "launcher array order is not sorted away"
        );
    }

    /// One failing member fails the whole slot: a partially-resolved member
    /// set would silently observe fewer pairings than the deployment wrote.
    #[test]
    fn one_bad_member_drops_the_whole_slot() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_1", "ghost_1"] } }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::UnknownInstanceId { .. })),
            "expected UnknownInstanceId, got {:?}",
            out.errors
        );
        assert!(
            out.planned.is_empty(),
            "the good member must not reach the plan on its own"
        );
    }

    /// Distinct target strings can name one pairing, so duplicates are caught
    /// after resolution rather than by comparing the raw strings.
    #[test]
    fn two_targets_resolving_to_one_pairing_are_rejected() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_deps = arm_deps();
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_1", "arm_1/controller"] } }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("robot_arm", &arm_instances, &arm_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        let error = out
            .errors
            .iter()
            .find(|e| matches!(e, ParsingError::DuplicateObservationTarget { .. }))
            .expect("expected DuplicateObservationTarget");
        assert!(
            error.to_string().contains("arm_1/controller"),
            "the message names the repeated pairing: {error}"
        );
        assert!(out.planned.is_empty());
    }

    /// A source observed through two of ITS OWN participant slots is two
    /// distinct members, not a duplicate.
    #[test]
    fn one_source_observed_through_two_of_its_slots_is_two_members() {
        let dual_instances = parse_instances(r#"[{ instance_id: "dual_1" }]"#);
        let dual_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl" }
            ]"#,
        );
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["dual_1/left_ctl", "dual_1/right_ctl"] } }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("dual_arm", &dual_instances, &dual_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            planned_sources(&out),
            [("dual_1", "left_ctl"), ("dual_1", "right_ctl")]
        );
    }

    /// Coverage follows the cardinality: every slot but `zero_or_more` needs an
    /// explicit `links` entry, while omitting `zero_or_more` IS its empty set.
    /// A `zero_or_one` slot is no exception: it may be emptied, but only by
    /// writing the vacancy down, so omitting it is still uncovered.
    #[test]
    fn omitted_observer_slots_error_per_cardinality() {
        for (cardinality, expect_uncovered) in [
            (None, true),
            (Some("zero_or_one"), true),
            (Some("one_or_more"), true),
            (Some("zero_or_more"), false),
        ] {
            let rec_instances = parse_instances(r#"[{ instance_id: "rec_1" }]"#);
            let rec_deps = recorder_deps_with(cardinality);
            let items = vec![observer_item("recorder", &rec_instances, &rec_deps)];
            let out = validate_observations(&items, &all_local());
            let uncovered = out
                .errors
                .iter()
                .any(|e| matches!(e, ParsingError::ObservationSlotUncovered(_)));
            assert_eq!(
                uncovered, expect_uncovered,
                "cardinality {cardinality:?}: unexpected coverage verdict, got {:?}",
                out.errors
            );
        }
    }

    /// A `zero_or_more` slot already spells its empty set as `[]`, so that is
    /// the spelling it keeps; `validate_link_slots` rejects the vacancy that
    /// would be a second one.
    #[test]
    fn a_zero_or_more_slot_writes_its_empty_set_as_an_array() {
        let rec_instances =
            parse_instances(r#"[{ instance_id: "rec_1", links: { observed_arm: [] } }]"#);
        let rec_deps = recorder_deps_with(Some("zero_or_more"));
        let items = vec![observer_item("recorder", &rec_instances, &rec_deps)];
        let out = validate_observations(&items, &all_local());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());
    }

    /// Coverage follows the cardinality, and a vacancy covers only where it is
    /// legal: a `one_or_more` slot has no empty state, so vacancy neither
    /// covers it nor is accepted for it (`validate_link_slots` rejects it),
    /// which is what keeps the generated "at least one pairing" docstring
    /// true.
    #[test]
    fn a_vacant_one_or_more_observer_is_not_covered() {
        let rec_instances = parse_instances(
            r#"[{
                instance_id: "rec_1",
                links: { observed_arm: { vacant: "nothing to watch" } }
            }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![observer_item("recorder", &rec_instances, &rec_deps)];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::ObservationSlotUncovered(_))),
            "a vacancy must not cover a `one_or_more` slot: {:?}",
            out.errors
        );
    }

    /// The sha256 pin and the ambiguity rule are per member, not per slot.
    #[test]
    fn per_member_sha_and_ambiguity_rules_hold_on_a_multi_slot() {
        let pinned_instances =
            parse_instances(r#"[{ instance_id: "arm_1" }, { instance_id: "arm_2" }]"#);
        let pinned_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", sha256: "bbb" }]"#,
        );
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["arm_1", "arm_2"] } }]"#,
        );
        let rec_deps = parse_observer_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm", cardinality: "one_or_more", sha256: "aaa" }]"#,
        );
        let items = vec![
            item("robot_arm", &pinned_instances, &pinned_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert_eq!(
            out.errors
                .iter()
                .filter(|e| matches!(e, ParsingError::PairingSha256Mismatch(_)))
                .count(),
            2,
            "each mismatching member reports its own pin clash: {:?}",
            out.errors
        );

        // Ambiguity is judged per member too: the pinned member resolves while
        // the bare one does not.
        let dual_instances = parse_instances(r#"[{ instance_id: "dual_1" }]"#);
        let dual_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl" }
            ]"#,
        );
        let rec_instances = parse_instances(
            r#"[{ instance_id: "rec_1", links: { observed_arm: ["dual_1/left_ctl", "dual_1"] } }]"#,
        );
        let rec_deps = recorder_deps_with(Some("one_or_more"));
        let items = vec![
            item("dual_arm", &dual_instances, &dual_deps),
            observer_item("recorder", &rec_instances, &rec_deps),
        ];
        let out = validate_observations(&items, &all_local());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::ObservationTargetAmbiguous(_))),
            "the unpinned member is still ambiguous: {:?}",
            out.errors
        );
        assert!(out.planned.is_empty());
    }
}
