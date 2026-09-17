//! Plan-phase validation for the participant-slot entries of the launcher's
//! per-instance `links` map (and the CLI's `--link` / `--vacant-link`, which
//! feed the same validator through the daemon). Producer-binding and observer
//! entries of the same `links` map are owned by `bindings` / `observations`;
//! this validator steps over them.
//!
//! A pair is one peer to one peer between two complementary slots (slot =
//! instance × link_id): same pairing `(name, tag)`, opposite roles. A slot
//! holds as many pairs as its `cardinality` admits: a `one` or `zero_or_one`
//! slot one, a `one_or_more` or `zero_or_more` slot any number, each pair
//! held once. Declaring a pair on ONE side covers both endpoints' slots;
//! declaring it from both sides is allowed but must agree. Every pairing slot
//! of every planned instance must end up paired, or, where the node's
//! manifest declares the slot `zero_or_one`, explicitly declared vacant, or,
//! where it declares `zero_or_more`, may hold nothing yet; otherwise the plan
//! is rejected (`PairingSlotUncovered`): no silent unpaired boots.

use crate::error::{
    PairAlreadyHeld, PairingConflict, PairingSha256Mismatch, PairingSlotAlreadyPaired,
    PairingSlotUncovered, PairingTargetAmbiguous, PairingTargetNotComplementary, ParsingError,
};
use config::node::{PairingObserverDependency, PairingParticipantDependency};
use std::collections::BTreeMap;

use super::types::{
    CardinalityShapeViolation, DeploymentInstance, check_cardinality_shape, split_link_target,
};

/// Minimal view of one node's planned (or already-running) instances needed
/// for pairing validation. Mirrors `BindingValidationItem` for the pairing
/// mechanism.
pub struct PairingValidationItem<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instances: &'a [DeploymentInstance],
    /// The node's declared participant slots (`depends_on.pairings`). Empty
    /// when the node declares none. Observer slots never participate in pairing
    /// establishment, exclusivity, or required-slot coverage, so they live in
    /// their own field and this validator never reads them.
    pub pairing_deps: &'a [PairingParticipantDependency],
    /// The node's declared observer slots (`depends_on.pairing_observers`),
    /// carried here so `observations` validates over the same item list.
    pub observer_deps: &'a [PairingObserverDependency],
    /// `true` for instances already running in the stack, folded in so they
    /// can serve as pair targets. Preexisting instances are exempt from the
    /// coverage rule (they were covered at their own launch) and their
    /// `pairings` maps are not re-processed.
    pub preexisting: bool,
}

/// One endpoint of a planned pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPairEndpoint {
    pub instance_id: String,
    pub link_id: String,
    pub role: String,
    /// The slot's declared cardinality, carried to a daemon that cannot read
    /// the manifest declaring it.
    pub cardinality: config::node::Cardinality,
}

/// One validated pair, ready to be applied when both endpoints reach
/// Running. `a` is the declaring side (deterministic; when both sides
/// declared, the lexicographically-first declaration wins as `a`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPairing {
    pub pairing_name: String,
    pub pairing_tag: String,
    pub a: PlannedPairEndpoint,
    pub b: PlannedPairEndpoint,
}

/// Outcome of [`validate_pairings`]: aggregated rule violations plus the
/// resolved pair plan. The caller must check `errors.is_empty()` before
/// consuming `planned`.
#[derive(Debug, Default)]
pub struct ValidatedPairings {
    pub errors: Vec<ParsingError>,
    pub planned: Vec<PlannedPairing>,
}

/// Pairs already established on running instances, keyed by `(instance_id,
/// link_id)`: the peer slots `(instance_id, link_id)` each slot holds right
/// now. A scalar slot listed here is taken; a multi slot listed here is open
/// to every peer it does not already hold.
pub type AlreadyPairedSlots = BTreeMap<(String, String), Vec<(String, String)>>;

/// `(instance_id, link_id)` slots this validator must treat as covered
/// although it can see no pair for them, because something outside its view
/// holds the other half.
///
/// Empty on the launcher and CLI paths, which see the whole plan. The daemon's
/// plan-phase check populates it: a slot a LATER-starting instance of the same
/// launch will claim, and a slot already paired to a peer on another machine,
/// are both paired facts that this daemon's snapshot cannot show. Neither is a
/// vacancy, so neither is written as one.
///
/// Keyed by `(instance_id, link_id)` like [`AlreadyPairedSlots`] and the
/// in-plan claim map, so external coverage granted to one instance can never
/// leak to a sibling deploying the same node.
pub type ExternallyCoveredSlots = std::collections::BTreeSet<(String, String)>;

/// The pairs claimed in this plan, keyed by slot: the peer slots each slot
/// takes part in a pair with, recorded in both directions.
type Claims = BTreeMap<(String, String), Vec<(String, String)>>;

/// Speaks a [`CardinalityShapeViolation`] in participant vocabulary, the way
/// `observations` and `bindings` speak it in theirs. A scalar slot given a set
/// keeps the existing `LinkTargetNotScalar`, whose message already covers both
/// the array and the repeated-flag spelling.
fn shape_error(violation: CardinalityShapeViolation, owner_id: &str, key: &str) -> ParsingError {
    let owner_instance_id = owner_id.to_string();
    let link = key.to_string();
    match violation {
        CardinalityShapeViolation::ArrayOnScalarSlot { .. }
        | CardinalityShapeViolation::SingleSlotMultipleTargets { .. } => {
            ParsingError::LinkTargetNotScalar {
                owner_instance_id,
                link,
            }
        }
        CardinalityShapeViolation::ScalarOnMultiSlot { cardinality } => {
            ParsingError::PairingScalarOnMultiSlot {
                owner_instance_id,
                link,
                cardinality,
            }
        }
        CardinalityShapeViolation::Unmet => ParsingError::PairingCardinalityUnmet {
            owner_instance_id,
            link,
        },
    }
}

/// How a peer slot is named in an error: `<instance>:<link_id>`.
fn peer_label(instance_id: &str, link_id: &str) -> String {
    format!("{instance_id}:{link_id}")
}

/// Run all pairing validator rules over the plan.
///
/// Rules:
/// 1. Only `links` keys that name one of this node's participant slots are
///    processed; every other key is skipped (a key naming no slot at all is
///    reported once by `validate_link_slots`). A scalar slot's value is one
///    target; an array on it is `LinkTargetNotScalar`. A multi slot's value
///    is one target per pair, in the order written.
/// 2. The target instance exists in the plan/stack (`UnknownInstanceId`).
/// 3. The target has exactly one available complementary slot — same
///    pairing `(name, tag)`, opposite role, open to this pair, or the
///    declaration names one via the `/<peer_link_id>` suffix
///    (`PairingTargetNotComplementary` / `PairingTargetAmbiguous`). A scalar
///    slot is open while nothing claims it; a multi slot is open to every
///    peer it does not hold.
/// 4. A scalar slot claimed twice in-plan, or already paired in the running
///    stack, is `PairingSlotAlreadyPaired`; a multi slot already holding the
///    pair declared is `PairAlreadyHeld`.
/// 5. Both-sides declarations must agree (`PairingConflict`).
/// 6. When both endpoints pin a `sha256` for the pairing document, the pins
///    must match (`PairingSha256Mismatch`).
/// 7. Coverage: every participant slot of every planned instance is paired,
///    declared vacant (`links: { <link_id>: { vacant: "<why>" } }`), or listed
///    in `externally_covered` (`PairingSlotUncovered` otherwise); a
///    `zero_or_more` slot may hold nothing. Whether a slot may be vacant at
///    all is `validate_link_slots`'s call.
pub fn validate_pairings(
    items: &[PairingValidationItem<'_>],
    already_paired: &AlreadyPairedSlots,
    externally_covered: &ExternallyCoveredSlots,
) -> ValidatedPairings {
    let mut out = ValidatedPairings::default();

    // instance_id → owning item.
    let mut lookup: BTreeMap<&str, &PairingValidationItem<'_>> = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            lookup.entry(instance.instance_id.as_str()).or_insert(item);
        }
    }

    let mut claims: Claims = BTreeMap::new();

    // Deterministic processing order: declarations sorted by owner
    // instance_id, then key (BTreeMap iteration gives key order; items are
    // walked in slice order, instances in slice order, so sort explicitly and
    // the resolution never depends on caller ordering). A multi slot's
    // targets keep the order they were written in.
    let mut declarations: Vec<(
        &DeploymentInstance,
        &PairingParticipantDependency,
        &str,
        &str,
    )> = Vec::new();
    for item in items.iter().filter(|i| !i.preexisting) {
        let participants_by_link: BTreeMap<&str, &PairingParticipantDependency> = item
            .pairing_deps
            .iter()
            .map(|dependency| (dependency.link_id.as_str(), dependency))
            .collect();
        for instance in item.instances {
            for (key, value) in &instance.links {
                // Only this node's participant slots establish pairs. Observer
                // slots and producer-binding slots share the `links` namespace
                // but are resolved by their own validators; a key naming no
                // slot at all is reported once by `validate_link_slots`.
                let Some(own_dep) = participants_by_link.get(key.as_str()).copied() else {
                    continue;
                };
                // A vacant slot claims nothing: it is the deployment saying
                // this slot has no peer, and `validate_coverage` reads it
                // there rather than here.
                let Some(selection) = value.selection() else {
                    continue;
                };
                if let Err(violation) = check_cardinality_shape(own_dep.cardinality, selection) {
                    out.errors
                        .push(shape_error(violation, instance.instance_id.as_str(), key));
                    continue;
                }
                for target in selection.targets() {
                    declarations.push((instance, own_dep, key.as_str(), target.as_str()));
                }
            }
        }
    }
    declarations
        .sort_by(|a, b| (a.0.instance_id.as_str(), a.2).cmp(&(b.0.instance_id.as_str(), b.2)));

    for (instance, own_dep, key, target) in declarations {
        match resolve_pair_declaration(
            instance.instance_id.as_str(),
            own_dep,
            key,
            target,
            &lookup,
            &claims,
            already_paired,
        ) {
            Ok(Some(pair)) => {
                let own_slot = (pair.a.instance_id.clone(), pair.a.link_id.clone());
                let peer_slot = (pair.b.instance_id.clone(), pair.b.link_id.clone());
                claims
                    .entry(own_slot.clone())
                    .or_default()
                    .push(peer_slot.clone());
                claims.entry(peer_slot).or_default().push(own_slot);
                out.planned.push(pair);
            }
            Ok(None) => {}
            Err(error) => out.errors.push(error),
        }
    }

    validate_coverage(items, &claims, externally_covered, &mut out.errors);

    out
}

/// The peers a slot holds in this plan and in the running stack, in that
/// order.
fn peers_of<'a>(
    slot: &(String, String),
    claims: &'a Claims,
    already_paired: &'a AlreadyPairedSlots,
) -> impl Iterator<Item = &'a (String, String)> {
    claims
        .get(slot)
        .into_iter()
        .flatten()
        .chain(already_paired.get(slot).into_iter().flatten())
}

/// Resolves ONE `pairings` declaration against the plan built so far (rules
/// 1-6 of [`validate_pairings`]): target lookup, complementary-slot
/// selection, claim/exclusivity checks, and the sha256 pin comparison.
///
/// `Ok(Some(_))` is a newly planned pair whose claims the caller records;
/// `Ok(None)` a reciprocal declaration agreeing with an already-planned pair
/// (nothing to add); `Err(_)` the rule violation this declaration hit.
fn resolve_pair_declaration(
    owner_id: &str,
    own_dep: &PairingParticipantDependency,
    key: &str,
    target: &str,
    lookup: &BTreeMap<&str, &PairingValidationItem<'_>>,
    claims: &Claims,
    already_paired: &AlreadyPairedSlots,
) -> Result<Option<PlannedPairing>, ParsingError> {
    let (target_instance, requested_peer_link) = split_link_target(target);
    let Some(target_item) = lookup.get(target_instance) else {
        return Err(ParsingError::UnknownInstanceId {
            owner_instance_id: owner_id.to_string(),
            link: key.to_string(),
            instance_id: target_instance.to_string(),
        });
    };

    let own_slot = (owner_id.to_string(), key.to_string());
    let own_scalar = own_dep.cardinality.is_scalar();

    // Error builders shared by the rule branches below (each is raised from
    // several sites with the same payload shape).
    let slot_taken = |instance_id: String, link_id: String, existing_peer: String| {
        ParsingError::PairingSlotAlreadyPaired(Box::new(PairingSlotAlreadyPaired {
            instance_id,
            link_id,
            existing_peer,
        }))
    };
    let pair_held = |instance_id: String, link_id: String, peer: String| {
        ParsingError::PairAlreadyHeld(Box::new(PairAlreadyHeld {
            instance_id,
            link_id,
            peer,
        }))
    };
    let not_complementary = || {
        ParsingError::PairingTargetNotComplementary(Box::new(PairingTargetNotComplementary {
            owner_instance_id: owner_id.to_string(),
            key: key.to_string(),
            target_instance_id: target_instance.to_string(),
            producer_name: target_item.node_name.to_string(),
            producer_tag: target_item.node_tag.to_string(),
            pairing_name: own_dep.name.as_str().to_string(),
            pairing_tag: own_dep.tag.clone(),
            role: own_dep.role.clone(),
        }))
    };

    // Both-sides agreement: the reciprocal declaration may have already
    // claimed our slot. Agreement = it claimed us against the same peer
    // instance (and slot, when we name one). On a scalar slot anything else
    // conflicts; a multi slot takes the disagreeing declaration as another
    // pair.
    if let Some(peers) = claims.get(&own_slot) {
        let agrees = peers.iter().any(|(claimed_inst, claimed_link)| {
            claimed_inst == target_instance && requested_peer_link.is_none_or(|l| l == claimed_link)
        });
        if agrees {
            return Ok(None);
        }
        if own_scalar {
            let (claimed_inst, claimed_link) = &peers[0];
            return Err(ParsingError::PairingConflict(Box::new(PairingConflict {
                instance_a: claimed_inst.clone(),
                link_a: claimed_link.clone(),
                target_a: format!("{owner_id}/{key}"),
                instance_b: owner_id.to_string(),
                link_b: key.to_string(),
                target_b: target.to_string(),
            })));
        }
    }
    if own_scalar && let Some((inst, link)) = already_paired.get(&own_slot).and_then(|p| p.first())
    {
        return Err(slot_taken(
            owner_id.to_string(),
            key.to_string(),
            peer_label(inst, link),
        ));
    }

    // Candidate peer slots: same pairing (name, tag), opposite role.
    let complementary: Vec<&PairingParticipantDependency> = target_item
        .pairing_deps
        .iter()
        .filter(|d| d.name == own_dep.name && d.tag == own_dep.tag && d.role != own_dep.role)
        .collect();
    // A scalar candidate is open while nothing claims or holds it; a multi
    // candidate is open to any peer.
    let open = |d: &PairingParticipantDependency| {
        let slot = (target_instance.to_string(), d.link_id.clone());
        !d.cardinality.is_scalar() || peers_of(&slot, claims, already_paired).next().is_none()
    };

    let resolved_peer_link = if let Some(peer_link) = requested_peer_link {
        // Explicit disambiguation: the named slot must exist and be
        // complementary; claimed-ness is reported precisely below.
        match complementary.iter().find(|d| d.link_id == peer_link) {
            Some(dep) => dep.link_id.clone(),
            None => return Err(not_complementary()),
        }
    } else {
        // No explicit slot: exactly one OPEN complementary slot must remain
        // (in-plan claim tracking).
        let available: Vec<&&PairingParticipantDependency> =
            complementary.iter().filter(|d| open(d)).collect();
        match available.as_slice() {
            [] => {
                // Distinguish "the target has no such slot at all" from
                // "its complementary slot(s) are taken". A slot taken by
                // another declaration in THIS plan is a disagreement
                // between declarations (PairingConflict); one taken in
                // the running stack is plain exclusivity.
                let taken_in_plan = complementary.iter().find_map(|d| {
                    let slot = (target_instance.to_string(), d.link_id.clone());
                    claims
                        .get(&slot)
                        .and_then(|peers| peers.first())
                        .map(|peer| (d.link_id.clone(), peer.clone()))
                });
                let taken_running = complementary.iter().find_map(|d| {
                    let slot = (target_instance.to_string(), d.link_id.clone());
                    already_paired
                        .get(&slot)
                        .and_then(|peers| peers.first())
                        .map(|(inst, link)| (d.link_id.clone(), peer_label(inst, link)))
                });
                return Err(
                    if let Some((taken_link, (peer_inst, peer_link))) = taken_in_plan {
                        ParsingError::PairingConflict(Box::new(PairingConflict {
                            instance_a: target_instance.to_string(),
                            link_a: taken_link,
                            // Same `<instance>/<link_id>` notation as the raw
                            // declaration in `target_b`, so one message never
                            // mixes peer-reference styles.
                            target_a: format!("{peer_inst}/{peer_link}"),
                            instance_b: owner_id.to_string(),
                            link_b: key.to_string(),
                            target_b: target.to_string(),
                        }))
                    } else if let Some((taken_link, peer_label)) = taken_running {
                        slot_taken(target_instance.to_string(), taken_link, peer_label)
                    } else {
                        not_complementary()
                    },
                );
            }
            [single] => single.link_id.clone(),
            multiple => {
                let candidates: Vec<&str> = multiple.iter().map(|d| d.link_id.as_str()).collect();
                return Err(ParsingError::PairingTargetAmbiguous(Box::new(
                    PairingTargetAmbiguous {
                        owner_instance_id: owner_id.to_string(),
                        key: key.to_string(),
                        target_instance_id: target_instance.to_string(),
                        pairing_name: own_dep.name.as_str().to_string(),
                        pairing_tag: own_dep.tag.clone(),
                        candidate_link_ids: candidates.join(", "),
                    },
                )));
            }
        }
    };
    let peer_dep = target_item
        .pairing_deps
        .iter()
        .find(|d| d.link_id == resolved_peer_link)
        .expect("resolved peer slot comes from target_item.pairing_deps");

    // Exclusivity of the resolved peer slot (reachable via the explicit
    // `/<peer_link_id>` path; the implicit path filtered taken slots). A
    // scalar peer slot is taken by any pair; a multi one already holding this
    // pair refuses it again.
    let peer_slot = (target_instance.to_string(), resolved_peer_link.clone());
    let mut held = peers_of(&peer_slot, claims, already_paired);
    if peer_dep.cardinality.is_scalar() {
        if let Some((inst, link)) = held.next() {
            return Err(slot_taken(
                target_instance.to_string(),
                resolved_peer_link.clone(),
                peer_label(inst, link),
            ));
        }
    } else if held.any(|(inst, link)| inst == owner_id && link == key) {
        return Err(pair_held(
            target_instance.to_string(),
            resolved_peer_link.clone(),
            peer_label(owner_id, key),
        ));
    }
    // The same pair held twice on this node's own multi slot.
    if peers_of(&own_slot, claims, already_paired)
        .any(|(inst, link)| inst == target_instance && *link == resolved_peer_link)
    {
        return Err(pair_held(
            owner_id.to_string(),
            key.to_string(),
            peer_label(target_instance, &resolved_peer_link),
        ));
    }

    // Rule 6: both-pinned sha256 must match.
    if let (Some(sha_own), Some(sha_peer)) = (&own_dep.sha256, &peer_dep.sha256)
        && sha_own != sha_peer
    {
        return Err(ParsingError::PairingSha256Mismatch(Box::new(
            PairingSha256Mismatch {
                instance_a: owner_id.to_string(),
                sha_a: sha_own.clone(),
                instance_b: target_instance.to_string(),
                sha_b: sha_peer.clone(),
                pairing_name: own_dep.name.as_str().to_string(),
                pairing_tag: own_dep.tag.clone(),
            },
        )));
    }

    Ok(Some(PlannedPairing {
        pairing_name: own_dep.name.as_str().to_string(),
        pairing_tag: own_dep.tag.clone(),
        a: PlannedPairEndpoint {
            instance_id: owner_id.to_string(),
            link_id: key.to_string(),
            role: own_dep.role.clone(),
            cardinality: own_dep.cardinality,
        },
        b: PlannedPairEndpoint {
            instance_id: target_instance.to_string(),
            link_id: resolved_peer_link,
            role: peer_dep.role.clone(),
            cardinality: peer_dep.cardinality,
        },
    }))
}

/// Rule 7 of [`validate_pairings`], over every planned (non-preexisting)
/// instance: each participant slot is paired in this plan, declared vacant,
/// covered outside this validator's view, or declared `zero_or_more`, which
/// may hold nothing. A `zero_or_one` slot is not exempt, only vacatable: a
/// slot with no `links` entry at all is uncovered whatever the manifest says,
/// so forgetting one stays an error and the cardinality only decides which
/// remedies the error offers.
fn validate_coverage(
    items: &[PairingValidationItem<'_>],
    claims: &Claims,
    externally_covered: &ExternallyCoveredSlots,
    errors: &mut Vec<ParsingError>,
) {
    for item in items.iter().filter(|i| !i.preexisting) {
        for instance in item.instances {
            let owner_id = instance.instance_id.as_str();
            for dep in item.pairing_deps {
                if dep.cardinality.allows_empty() {
                    continue;
                }
                let slot = (owner_id.to_string(), dep.link_id.clone());
                let covered = claims.contains_key(&slot)
                    || externally_covered.contains(&slot)
                    || instance
                        .links
                        .get(&dep.link_id)
                        .is_some_and(|value| value.vacancy().is_some());
                if !covered {
                    errors.push(ParsingError::PairingSlotUncovered(Box::new(
                        PairingSlotUncovered {
                            instance_id: owner_id.to_string(),
                            link_id: dep.link_id.clone(),
                            pairing_name: dep.name.as_str().to_string(),
                            pairing_tag: dep.tag.clone(),
                            role: dep.role.clone(),
                            cardinality: dep.cardinality,
                        },
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_pairing_deps(json5: &str) -> Vec<PairingParticipantDependency> {
        serde_json5::from_str(json5).expect("pairing deps fixture should parse")
    }

    fn arm_deps() -> Vec<PairingParticipantDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]"#,
        )
    }

    fn controller_deps() -> Vec<PairingParticipantDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }]"#,
        )
    }

    /// Every launcher/CLI plan holds both endpoints of every pair, so these
    /// tests never have slots covered outside the validator's view; the
    /// daemon's plan-phase check is the one caller that does.
    fn validate(
        items: &[PairingValidationItem<'_>],
        already_paired: &AlreadyPairedSlots,
    ) -> ValidatedPairings {
        validate_pairings(items, already_paired, &ExternallyCoveredSlots::new())
    }

    fn item<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        pairing_deps: &'a [PairingParticipantDependency],
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

    fn preexisting<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        pairing_deps: &'a [PairingParticipantDependency],
    ) -> PairingValidationItem<'a> {
        PairingValidationItem {
            preexisting: true,
            ..item(node_name, instances, pairing_deps)
        }
    }

    /// A multi participant slot takes one target per pair it holds, so a
    /// scalar is refused there exactly as it is on an observer or a
    /// producer-binding slot. All three families ask one shape rule.
    #[test]
    fn a_scalar_on_a_multi_participant_slot_is_refused() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let engine_instances =
            parse_instances(r#"[{ instance_id: "engine_1", links: { controllers: "arm_1" } }]"#);
        let engine_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "controllers", cardinality: "zero_or_more" }]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("sim_engine", &engine_instances, &engine_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| e.to_string().contains("controllers")
                    && e.to_string().contains("link an array")),
            "the refusal names the slot and what to write instead: {:?}",
            out.errors
        );
        assert!(out.planned.is_empty(), "a refused shape plans no pair");
    }

    /// An empty array on a `one_or_more` participant slot names no peer, so
    /// the slot's floor is unmet and the plan says which spelling means
    /// "may hold nothing" instead.
    #[test]
    fn an_empty_array_on_a_one_or_more_participant_slot_is_refused() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let engine_instances =
            parse_instances(r#"[{ instance_id: "engine_1", links: { controllers: [] } }]"#);
        let engine_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "controllers", cardinality: "one_or_more" }]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("sim_engine", &engine_instances, &engine_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| e.to_string().contains("controllers")
                    && e.to_string().contains("at least one peer")),
            "the refusal names the slot and its minimum: {:?}",
            out.errors
        );
        assert!(out.planned.is_empty(), "a refused shape plans no pair");
    }

    /// The shared rule reports two distinct violations, an array on a scalar
    /// slot and repeated flags on one, through the single-target error that
    /// already spells both. This guards that collapsing.
    #[test]
    fn an_array_on_a_scalar_participant_slot_still_names_the_single_target_rule() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: ["arm_1", "arm_2"] } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| e.to_string().contains("takes a single")),
            "a scalar slot given a set keeps the single-target error: {:?}",
            out.errors
        );
    }

    #[test]
    fn one_sided_declaration_pairs_both_slots() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
        let pair = &out.planned[0];
        assert_eq!(pair.pairing_name, "arm_link");
        assert_eq!(pair.a.instance_id, "ctrl_1");
        assert_eq!(pair.a.link_id, "arm");
        assert_eq!(pair.a.role, "controller");
        assert_eq!(pair.b.instance_id, "arm_1");
        assert_eq!(pair.b.link_id, "controller");
        assert_eq!(pair.b.role, "arm");
    }

    #[test]
    fn both_sides_declared_and_agreeing_dedupes_to_one_pair() {
        let arm_instances =
            parse_instances(r#"[{ instance_id: "arm_1", links: { controller: "ctrl_1" } }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1, "agreeing pair must dedupe");
    }

    #[test]
    fn both_sides_disagreeing_is_a_conflict() {
        let arm_instances = parse_instances(
            r#"[
                { instance_id: "arm_1", links: { controller: "ctrl_2" } },
                { instance_id: "arm_2" }
            ]"#,
        );
        let arm_pairing_deps = arm_deps();
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "ctrl_1", links: { arm: "arm_1" } },
                { instance_id: "ctrl_2" }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingConflict(_))),
            "expected PairingConflict, got {:?}",
            out.errors
        );
    }

    /// A `links` key that names no participant slot is not this validator's
    /// concern: it is silently skipped (the unified `validate_link_slots`
    /// reports an unknown key). The instance's own required slot stays
    /// uncovered, which is the error that surfaces here.
    #[test]
    fn non_participant_key_is_skipped() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { ghost_slot: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .all(|e| !matches!(e, ParsingError::PairingConflict(_))),
            "a non-participant key must not be treated as a pairing declaration: {:?}",
            out.errors
        );
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingSlotUncovered(_))),
            "ctrl_1's required `arm` slot stays uncovered: {:?}",
            out.errors
        );
    }

    #[test]
    fn unknown_target_instance_is_rejected() {
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "ghost" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::UnknownInstanceId { .. })),
            "expected UnknownInstanceId, got {:?}",
            out.errors
        );
    }

    #[test]
    fn non_complementary_target_is_rejected() {
        // Target declares the SAME role — never complementary.
        let other_ctrl_instances = parse_instances(r#"[{ instance_id: "other_ctrl" }]"#);
        let other_ctrl_deps = controller_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "other_ctrl" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("other_controller", &other_ctrl_instances, &other_ctrl_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingTargetNotComplementary(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingTargetNotComplementary");
        assert_eq!(info.target_instance_id, "other_ctrl");
        assert_eq!(info.pairing_name, "arm_link");
        assert_eq!(info.role, "controller");
    }

    #[test]
    fn ambiguous_target_requires_peer_link_disambiguation() {
        // A dual-role node with two complementary 'arm' slots. Neither is paired
        // in this half, so both are declared vacant to keep the plan covered.
        let dual_arm_instances = parse_instances(
            r#"[{
                instance_id: "dual_1",
                links: {
                    left_ctl: { vacant: "this half is unwired in the fixture" },
                    right_ctl: { vacant: "this half is unwired in the fixture" }
                }
            }]"#,
        );
        let dual_arm_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl" }
            ]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "dual_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("dual_arm", &dual_arm_instances, &dual_arm_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingTargetAmbiguous(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingTargetAmbiguous");
        assert_eq!(info.candidate_link_ids, "left_ctl, right_ctl");
        assert!(
            info.to_string().contains("/<peer_link_id>"),
            "hint should mention the disambiguation syntax: {info}"
        );

        // Explicit disambiguation resolves it: the pinned slot is paired, and
        // only the other one stays deliberately empty.
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "dual_1/left_ctl" } }]"#);
        let dual_arm_instances = parse_instances(
            r#"[{
                instance_id: "dual_1",
                links: { right_ctl: { vacant: "only the left half is wired here" } }
            }]"#,
        );
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("dual_arm", &dual_arm_instances, &dual_arm_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
        assert_eq!(out.planned[0].b.link_id, "left_ctl");
    }

    #[test]
    fn in_plan_exclusivity_rejects_second_claim_on_same_slot() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "ctrl_1", links: { arm: "arm_1" } },
                { instance_id: "ctrl_2", links: { arm: "arm_1" } }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        // ctrl_1 wins (deterministic order); ctrl_2's claim collides with
        // the in-plan pair and is reported as a conflict naming it.
        assert_eq!(out.planned.len(), 1);
        assert_eq!(out.planned[0].a.instance_id, "ctrl_1");
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingConflict(_))),
            "expected a PairingConflict naming the winning pair, got {:?}",
            out.errors
        );
    }

    #[test]
    fn already_paired_running_slot_is_exclusive() {
        // arm_1 is running and its slot is already paired (e.g. to a live
        // controller); a new controller naming it explicitly must be told.
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_2", links: { arm: "arm_1/controller" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            preexisting("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let already: AlreadyPairedSlots = [(
            ("arm_1".to_string(), "controller".to_string()),
            vec![("ctrl_1".to_string(), "arm".to_string())],
        )]
        .into_iter()
        .collect();
        let out = validate(&items, &already);
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingSlotAlreadyPaired(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingSlotAlreadyPaired");
        assert_eq!(info.instance_id, "arm_1");
        assert_eq!(info.link_id, "controller");
        assert_eq!(info.existing_peer, "ctrl_1:arm");
    }

    /// A required slot's uncovered message offers a peer and the manifest key
    /// that would let the slot run without one, never the vacancy spelling the
    /// manifest forbids: an error must not advertise an escape hatch that would
    /// be rejected the moment it is written.
    #[test]
    fn required_slot_uncovered_fails_loudly() {
        let ctrl_instances = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate(&items, &BTreeMap::new());
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingSlotUncovered(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingSlotUncovered");
        assert_eq!(info.instance_id, "ctrl_1");
        assert_eq!(info.link_id, "arm");
        assert_eq!(info.role, "controller");
        let msg = info.to_string();
        assert!(
            msg.contains("cardinality `one`") && msg.contains("--link arm@"),
            "message should show the fix: {msg}"
        );
        assert!(
            msg.contains("`cardinality: \"zero_or_one\"`"),
            "message should name the manifest key that waives the peer: {msg}"
        );
        assert!(
            msg.contains("if it is meant to run empty, declare"),
            "the vacancy must be offered only behind the manifest change, never as a \
             standalone fix: {msg}"
        );
    }

    /// A `zero_or_one` slot is vacatable, not exempt: forgetting it is the
    /// same error, and only its remedy list differs.
    #[test]
    fn optional_slot_is_uncovered_when_forgotten_and_covered_when_vacated() {
        let optional_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm", cardinality: "zero_or_one" }]"#,
        );

        let forgotten = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let out = validate(
            &[item("arm_controller", &forgotten, &optional_deps)],
            &BTreeMap::new(),
        );
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingSlotUncovered(info) => Some(info),
                _ => None,
            })
            .expect("a forgotten optional slot is still uncovered");
        let msg = info.to_string();
        assert!(
            msg.contains("cardinality `zero_or_one`") && msg.contains("--vacant-link 'arm=<why>'"),
            "a zero_or_one slot's message offers both remedies: {msg}"
        );

        let vacated = parse_instances(
            r#"[{ instance_id: "ctrl_1", links: { arm: { vacant: "monitor rig: nothing to command" } } }]"#,
        );
        let out = validate(
            &[item("arm_controller", &vacated, &optional_deps)],
            &BTreeMap::new(),
        );
        assert!(
            out.errors.is_empty(),
            "a vacated optional slot is covered: {:?}",
            out.errors
        );
        assert!(out.planned.is_empty(), "a vacancy claims no peer");
    }

    /// A vacant participant slot covers itself, claims nothing, and plans no
    /// pair.
    #[test]
    fn a_vacant_required_slot_is_covered() {
        let ctrl_instances = parse_instances(
            r#"[{
                instance_id: "ctrl_1",
                links: { arm: { vacant: "bench rig: no arm wired to this panel" } }
            }]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());
    }

    /// Two instances of ONE node in one deployment, one paired and one
    /// vacant: the fate of a slot is per instance, which is the whole reason
    /// it is written in the launcher rather than the manifest.
    #[test]
    fn sibling_instances_of_one_node_choose_their_own_slot_fates() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "governed_ctl", links: { arm: "arm_1" } },
                {
                    instance_id: "monitor_ctl",
                    links: { arm: { vacant: "monitor rig: nothing commands this panel" } }
                }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1, "only the paired sibling plans a pair");
        assert_eq!(out.planned[0].a.instance_id, "governed_ctl");
        assert_eq!(out.planned[0].b.instance_id, "arm_1");
    }

    /// A slot the validator cannot see a pair for, but whose other half the
    /// caller holds, is covered without being a vacancy: the daemon's
    /// plan-phase check is the caller that knows this.
    #[test]
    fn an_externally_covered_slot_is_covered_without_being_vacant() {
        let ctrl_instances = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];

        let out = validate(&items, &BTreeMap::new());
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingSlotUncovered(_))),
            "without external coverage the slot is uncovered: {:?}",
            out.errors
        );

        let externally_covered: ExternallyCoveredSlots =
            [("ctrl_1".to_string(), "arm".to_string())]
                .into_iter()
                .collect();
        let out = validate_pairings(&items, &BTreeMap::new(), &externally_covered);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());
    }

    /// External coverage is per SLOT, not per node: a sibling instance of the
    /// same node does not inherit it.
    #[test]
    fn external_coverage_does_not_leak_to_a_sibling_instance() {
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1" }, { instance_id: "ctrl_2" }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let externally_covered: ExternallyCoveredSlots =
            [("ctrl_1".to_string(), "arm".to_string())]
                .into_iter()
                .collect();
        let out = validate_pairings(&items, &BTreeMap::new(), &externally_covered);
        let uncovered: Vec<&str> = out
            .errors
            .iter()
            .filter_map(|e| match e {
                ParsingError::PairingSlotUncovered(info) => Some(info.instance_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(uncovered, ["ctrl_2"]);
    }

    #[test]
    fn sha256_mismatch_between_pinned_sides_is_rejected() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", sha256: "aaa" }]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm", sha256: "bbb" }]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingSha256Mismatch(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingSha256Mismatch");
        assert_eq!(info.sha_a, "bbb");
        assert_eq!(info.sha_b, "aaa");
    }

    #[test]
    fn one_sided_sha_pin_is_accepted() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", sha256: "aaa" }]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
    }

    #[test]
    fn two_arm_commander_pairs_left_and_right_isolated() {
        let arm_instances = parse_instances(
            r#"[
                { instance_id: "left_arm_inst" },
                { instance_id: "right_arm_inst" }
            ]"#,
        );
        let arm_pairing_deps = arm_deps();
        let commander_instances = parse_instances(
            r#"[{
                instance_id: "commander",
                links: { left_arm: "left_arm_inst", right_arm: "right_arm_inst" }
            }]"#,
        );
        let commander_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "controller", link_id: "left_arm" },
                { name: "arm_link", tag: "v1", role: "controller", link_id: "right_arm" }
            ]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("two_arm_commander", &commander_instances, &commander_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 2);
        let left = out
            .planned
            .iter()
            .find(|p| p.a.link_id == "left_arm")
            .expect("left pair");
        assert_eq!(left.b.instance_id, "left_arm_inst");
        let right = out
            .planned
            .iter()
            .find(|p| p.a.link_id == "right_arm")
            .expect("right pair");
        assert_eq!(right.b.instance_id, "right_arm_inst");
    }

    #[test]
    fn preexisting_instances_are_valid_targets_but_not_coverage_checked() {
        // arm_1 runs already; ctrl_1 launches with a pair naming it. ctrl_1's
        // slot is covered by its own claim, and arm_1's slot must not be
        // coverage-checked at all (it was covered at arm_1's own launch).
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps();
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            preexisting("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
    }

    fn engine_deps() -> Vec<PairingParticipantDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controllers", cardinality: "zero_or_more" }]"#,
        )
    }

    /// A multi slot takes one target per pair: two controllers naming the
    /// engine pair into its one `controllers` slot, and the engine itself may
    /// name both in an array.
    #[test]
    fn a_multi_slot_holds_one_pair_per_target() {
        let engine_instances = parse_instances(r#"[{ instance_id: "engine" }]"#);
        let engine_pairing_deps = engine_deps();
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "ctrl_1", links: { arm: "engine" } },
                { instance_id: "ctrl_2", links: { arm: "engine" } }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("sim_engine", &engine_instances, &engine_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 2);
        assert!(
            out.planned
                .iter()
                .all(|pair| pair.b.instance_id == "engine" && pair.b.link_id == "controllers")
        );

        let engine_instances = parse_instances(
            r#"[{ instance_id: "engine", links: { controllers: ["ctrl_1", "ctrl_2"] } }]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1" }, { instance_id: "ctrl_2" }]"#);
        let items = vec![
            item("sim_engine", &engine_instances, &engine_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &BTreeMap::new());
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            out.planned
                .iter()
                .map(|pair| pair.b.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["ctrl_1", "ctrl_2"],
            "pairs keep the order the array wrote"
        );
    }

    /// A running multi slot stays open to a new peer and closed to a peer it
    /// already holds; a scalar slot takes one target only.
    #[test]
    fn a_running_multi_slot_takes_new_peers_and_refuses_a_held_one() {
        let engine_instances = parse_instances(r#"[{ instance_id: "engine" }]"#);
        let engine_pairing_deps = engine_deps();
        let ctrl_pairing_deps = controller_deps();
        let already: AlreadyPairedSlots = [(
            ("engine".to_string(), "controllers".to_string()),
            vec![("ctrl_1".to_string(), "arm".to_string())],
        )]
        .into_iter()
        .collect();

        let joining = parse_instances(r#"[{ instance_id: "ctrl_2", links: { arm: "engine" } }]"#);
        let items = vec![
            preexisting("sim_engine", &engine_instances, &engine_pairing_deps),
            item("arm_controller", &joining, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &already);
        assert!(
            out.errors.is_empty(),
            "a held multi slot stays open: {:?}",
            out.errors
        );
        assert_eq!(out.planned.len(), 1);

        let rejoining = parse_instances(r#"[{ instance_id: "ctrl_1", links: { arm: "engine" } }]"#);
        let items = vec![
            preexisting("sim_engine", &engine_instances, &engine_pairing_deps),
            item("arm_controller", &rejoining, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &already);
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairAlreadyHeld(info) => Some(info),
                _ => None,
            })
            .expect("the same pair twice is refused");
        assert_eq!(
            (info.instance_id.as_str(), info.link_id.as_str()),
            ("engine", "controllers")
        );
        assert_eq!(info.peer, "ctrl_1:arm");
        assert!(
            info.to_string()
                .contains("holds each pair once until it is cleared"),
            "the refusal states the multi slot's rule: {info}"
        );

        let arrayed = parse_instances(r#"[{ instance_id: "ctrl_3", links: { arm: ["engine"] } }]"#);
        let items = vec![
            preexisting("sim_engine", &engine_instances, &engine_pairing_deps),
            item("arm_controller", &arrayed, &ctrl_pairing_deps),
        ];
        let out = validate(&items, &already);
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::LinkTargetNotScalar { .. })),
            "a scalar slot takes one target: {:?}",
            out.errors
        );
    }

    /// A `zero_or_more` slot is covered holding nothing; a `one_or_more` slot
    /// with no pair is uncovered and pointed at the cardinality that may hold
    /// nothing.
    #[test]
    fn coverage_follows_the_slot_cardinality() {
        let engine_instances = parse_instances(r#"[{ instance_id: "engine" }]"#);
        let engine_pairing_deps = engine_deps();
        let out = validate(
            &[item("sim_engine", &engine_instances, &engine_pairing_deps)],
            &BTreeMap::new(),
        );
        assert!(
            out.errors.is_empty(),
            "zero_or_more holds nothing: {:?}",
            out.errors
        );

        let floored_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controllers", cardinality: "one_or_more" }]"#,
        );
        let out = validate(
            &[item("sim_engine", &engine_instances, &floored_deps)],
            &BTreeMap::new(),
        );
        let info = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingSlotUncovered(info) => Some(info),
                _ => None,
            })
            .expect("a one_or_more slot with no pair is uncovered");
        let msg = info.to_string();
        assert!(
            msg.contains("cardinality `one_or_more`")
                && msg.contains("[\"<peer_instance>\", ...]")
                && msg.contains("`cardinality: \"zero_or_more\"`"),
            "{msg}"
        );
    }
}
