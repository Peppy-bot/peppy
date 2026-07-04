//! Plan-phase validation for the launcher's per-instance `pairings` /
//! `defer_pairings` fields (and the CLI's `--pair` / `--defer-pair`, which
//! feed the same validator through the daemon).
//!
//! A pair is strictly 1:1 between two complementary slots (slot = instance ×
//! link_id): same pairing `(name, tag)`, opposite roles, both unclaimed.
//! Declaring the pair on ONE side covers both endpoints' slots; declaring it
//! from both sides is allowed but must agree. Every REQUIRED pairing slot of
//! every planned instance must end up paired or explicitly deferred, or the
//! plan is rejected (`PairingSlotUncovered`) — no silent unpaired boots.
//! `optional: true` slots are exempt: they boot unpaired without ceremony.

use crate::error::{
    PairingConflict, PairingDeadKey, PairingSha256Mismatch, PairingSlotAlreadyPaired,
    PairingSlotUncovered, PairingTargetAmbiguous, PairingTargetNotComplementary, ParsingError,
};
use config::node::PairingDependency;
use std::collections::BTreeMap;

use super::types::{DeploymentInstance, split_pair_target};

/// Minimal view of one node's planned (or already-running) instances needed
/// for pairing validation. Mirrors `BindingValidationItem` for the pairing
/// mechanism.
pub struct PairingValidationItem<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instances: &'a [DeploymentInstance],
    /// The node's declared pairing slots (`depends_on.pairings`). Empty when
    /// the node declares none.
    pub pairing_deps: &'a [PairingDependency],
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

/// Slots of already-running instances that are exclusively claimed right
/// now, keyed by `(instance_id, link_id)`; the value is a human-readable
/// peer label for error messages.
pub type AlreadyPairedSlots = BTreeMap<(String, String), String>;

/// How `defer_pairings` entries naming an `optional` slot are treated.
///
/// User-facing surfaces (launcher files, the CLI preflight) reject them
/// ([`Self::Reject`]): optional slots boot unpaired without ceremony, so
/// deferring one is a user error worth flagging. The daemon's `node_run`
/// re-check accepts them ([`Self::Allow`]): `stack launch` auto-defers the
/// earlier endpoint of every planned pair — optional slots included — and
/// those mechanism-generated defers ride the same goal field as user
/// `--defer-pair`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalDeferPolicy {
    Reject,
    Allow,
}

/// Run all pairing validator rules over the plan.
///
/// Rules:
/// 1. Every `pairings` key matches a declared pairing slot link_id
///    (`PairingDeadKey`).
/// 2. The target instance exists in the plan/stack (`UnknownInstanceId`).
/// 3. The target has exactly one available complementary slot — same
///    pairing `(name, tag)`, opposite role, unclaimed — or the declaration
///    names one via the `/<peer_link_id>` suffix
///    (`PairingTargetNotComplementary` / `PairingTargetAmbiguous`).
/// 4. Slots are exclusive: a slot claimed twice in-plan, or already paired
///    in the running stack, is `PairingSlotAlreadyPaired`.
/// 5. Both-sides declarations must agree (`PairingConflict`).
/// 6. When both endpoints pin a `sha256` for the pairing document, the pins
///    must match (`PairingSha256Mismatch`).
/// 7. Coverage: every required slot of every planned instance is paired or
///    listed in `defer_pairings` (`PairingSlotUncovered`); a
///    `defer_pairings` entry naming an unknown slot, a slot that is also
///    paired, or — under [`OptionalDeferPolicy::Reject`] — an optional slot
///    is `PairingDeferInvalid`.
pub fn validate_pairings(
    items: &[PairingValidationItem<'_>],
    already_paired: &AlreadyPairedSlots,
    optional_defers: OptionalDeferPolicy,
) -> ValidatedPairings {
    let mut out = ValidatedPairings::default();

    // instance_id → owning item.
    let mut lookup: BTreeMap<&str, &PairingValidationItem<'_>> = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            lookup.entry(instance.instance_id.as_str()).or_insert(item);
        }
    }

    // (instance_id, link_id) → the peer (instance_id, link_id) claimed by
    // this plan. Both directions are recorded per pair.
    let mut claims: BTreeMap<(String, String), (String, String)> = BTreeMap::new();

    // Deterministic processing order: declarations sorted by owner
    // instance_id, then key (BTreeMap iteration gives key order; items are
    // walked in slice order, instances in slice order — sort explicitly so
    // the resolution never depends on caller ordering).
    let mut declarations: Vec<(&DeploymentInstance, &PairingValidationItem<'_>, &str, &str)> =
        Vec::new();
    for item in items.iter().filter(|i| !i.preexisting) {
        for instance in item.instances {
            for (key, target) in &instance.pairings {
                declarations.push((instance, item, key.as_str(), target.as_str()));
            }
        }
    }
    declarations
        .sort_by(|a, b| (a.0.instance_id.as_str(), a.2).cmp(&(b.0.instance_id.as_str(), b.2)));

    for (instance, item, key, target) in declarations {
        match resolve_pair_declaration(
            instance.instance_id.as_str(),
            item,
            key,
            target,
            &lookup,
            &claims,
            already_paired,
        ) {
            Ok(Some(pair)) => {
                let own_slot = (pair.a.instance_id.clone(), pair.a.link_id.clone());
                let peer_slot = (pair.b.instance_id.clone(), pair.b.link_id.clone());
                claims.insert(own_slot.clone(), peer_slot.clone());
                claims.insert(peer_slot, own_slot);
                out.planned.push(pair);
            }
            Ok(None) => {}
            Err(error) => out.errors.push(error),
        }
    }

    validate_coverage_and_defers(items, &claims, optional_defers, &mut out.errors);

    out
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
    item: &PairingValidationItem<'_>,
    key: &str,
    target: &str,
    lookup: &BTreeMap<&str, &PairingValidationItem<'_>>,
    claims: &BTreeMap<(String, String), (String, String)>,
    already_paired: &AlreadyPairedSlots,
) -> Result<Option<PlannedPairing>, ParsingError> {
    let Some(own_dep) = item.pairing_deps.iter().find(|d| d.link_id == key) else {
        let declared: Vec<&str> = item
            .pairing_deps
            .iter()
            .map(|d| d.link_id.as_str())
            .collect();
        return Err(ParsingError::PairingDeadKey(Box::new(PairingDeadKey {
            owner_instance_id: owner_id.to_string(),
            key: key.to_string(),
            declared_link_ids: declared.join(", "),
        })));
    };

    let (target_instance, requested_peer_link) = split_pair_target(target);
    let Some(target_item) = lookup.get(target_instance) else {
        return Err(ParsingError::UnknownInstanceId {
            owner_instance_id: owner_id.to_string(),
            binding: key.to_string(),
            instance_id: target_instance.to_string(),
        });
    };

    let own_slot = (owner_id.to_string(), key.to_string());

    // Error builders shared by the rule branches below (each is raised from
    // several sites with the same payload shape).
    let slot_taken = |instance_id: String, link_id: String, existing_peer: String| {
        ParsingError::PairingSlotAlreadyPaired(Box::new(PairingSlotAlreadyPaired {
            instance_id,
            link_id,
            existing_peer,
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
    // instance (and slot, when we name one); anything else conflicts.
    if let Some((claimed_peer_inst, claimed_peer_link)) = claims.get(&own_slot) {
        let agrees = claimed_peer_inst == target_instance
            && requested_peer_link.is_none_or(|l| l == claimed_peer_link);
        if agrees {
            return Ok(None);
        }
        return Err(ParsingError::PairingConflict(Box::new(PairingConflict {
            instance_a: claimed_peer_inst.clone(),
            link_a: claimed_peer_link.clone(),
            target_a: format!("{owner_id}/{key}"),
            instance_b: owner_id.to_string(),
            link_b: key.to_string(),
            target_b: target.to_string(),
        })));
    }
    if let Some(peer_label) = already_paired.get(&own_slot) {
        return Err(slot_taken(
            owner_id.to_string(),
            key.to_string(),
            peer_label.clone(),
        ));
    }

    // Candidate peer slots: same pairing (name, tag), opposite role.
    let complementary: Vec<&PairingDependency> = target_item
        .pairing_deps
        .iter()
        .filter(|d| d.name == own_dep.name && d.tag == own_dep.tag && d.role != own_dep.role)
        .collect();

    let resolved_peer_link = if let Some(peer_link) = requested_peer_link {
        // Explicit disambiguation: the named slot must exist and be
        // complementary; claimed-ness is reported precisely below.
        match complementary.iter().find(|d| d.link_id == peer_link) {
            Some(dep) => dep.link_id.clone(),
            None => return Err(not_complementary()),
        }
    } else {
        // No explicit slot: exactly one AVAILABLE complementary slot
        // must remain (in-plan claim tracking).
        let available: Vec<&&PairingDependency> = complementary
            .iter()
            .filter(|d| {
                let slot = (target_instance.to_string(), d.link_id.clone());
                !claims.contains_key(&slot) && !already_paired.contains_key(&slot)
            })
            .collect();
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
                        .map(|peer| (d.link_id.clone(), peer.clone()))
                });
                let taken_running = complementary.iter().find_map(|d| {
                    let slot = (target_instance.to_string(), d.link_id.clone());
                    already_paired
                        .get(&slot)
                        .map(|label| (d.link_id.clone(), label.clone()))
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

    // Exclusivity of the resolved peer slot (reachable via the explicit
    // `/<peer_link_id>` path; the implicit path filtered claimed slots).
    let peer_slot = (target_instance.to_string(), resolved_peer_link.clone());
    if let Some((existing_inst, existing_link)) = claims.get(&peer_slot) {
        return Err(slot_taken(
            target_instance.to_string(),
            resolved_peer_link.clone(),
            format!("{existing_inst}:{existing_link}"),
        ));
    }
    if let Some(peer_label) = already_paired.get(&peer_slot) {
        return Err(slot_taken(
            target_instance.to_string(),
            resolved_peer_link.clone(),
            peer_label.clone(),
        ));
    }

    // Rule 6: both-pinned sha256 must match.
    let peer_dep = target_item
        .pairing_deps
        .iter()
        .find(|d| d.link_id == resolved_peer_link)
        .expect("resolved peer slot comes from target_item.pairing_deps");
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
        },
        b: PlannedPairEndpoint {
            instance_id: target_instance.to_string(),
            link_id: resolved_peer_link,
            role: peer_dep.role.clone(),
        },
    }))
}

/// Rule 7 of [`validate_pairings`], over every planned (non-preexisting)
/// instance: each required slot must be paired or deferred, and each
/// `defer_pairings` entry must name a known slot that is not also paired in
/// the same plan (nor optional, when `optional_defers` rejects those).
fn validate_coverage_and_defers(
    items: &[PairingValidationItem<'_>],
    claims: &BTreeMap<(String, String), (String, String)>,
    optional_defers: OptionalDeferPolicy,
    errors: &mut Vec<ParsingError>,
) {
    for item in items.iter().filter(|i| !i.preexisting) {
        for instance in item.instances {
            let owner_id = instance.instance_id.as_str();
            for link_id in &instance.defer_pairings {
                let dep = item.pairing_deps.iter().find(|d| &d.link_id == link_id);
                let reason = match dep {
                    None => Some("no such pairing slot is declared".to_string()),
                    Some(_) if claims.contains_key(&(owner_id.to_string(), link_id.clone())) => {
                        Some("the slot is also paired in this plan".to_string())
                    }
                    Some(dep) if dep.optional && optional_defers == OptionalDeferPolicy::Reject => {
                        Some("the slot is optional and boots unpaired without deferral".to_string())
                    }
                    Some(_) => None,
                };
                if let Some(reason) = reason {
                    errors.push(ParsingError::PairingDeferInvalid {
                        owner_instance_id: owner_id.to_string(),
                        link_id: link_id.clone(),
                        reason,
                    });
                }
            }
            for dep in item.pairing_deps {
                if dep.optional {
                    continue;
                }
                let covered = claims.contains_key(&(owner_id.to_string(), dep.link_id.clone()))
                    || instance.defer_pairings.contains(&dep.link_id);
                if !covered {
                    errors.push(ParsingError::PairingSlotUncovered(Box::new(
                        PairingSlotUncovered {
                            instance_id: owner_id.to_string(),
                            link_id: dep.link_id.clone(),
                            pairing_name: dep.name.as_str().to_string(),
                            pairing_tag: dep.tag.clone(),
                            role: dep.role.clone(),
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

    fn parse_pairing_deps(json5: &str) -> Vec<PairingDependency> {
        serde_json5::from_str(json5).expect("pairing deps fixture should parse")
    }

    fn arm_deps(optional: bool) -> Vec<PairingDependency> {
        parse_pairing_deps(&format!(
            r#"[{{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: {optional} }}]"#
        ))
    }

    fn controller_deps() -> Vec<PairingDependency> {
        parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }]"#,
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

    fn preexisting<'a>(
        node_name: &'a str,
        instances: &'a [DeploymentInstance],
        pairing_deps: &'a [PairingDependency],
    ) -> PairingValidationItem<'a> {
        PairingValidationItem {
            preexisting: true,
            ..item(node_name, instances, pairing_deps)
        }
    }

    #[test]
    fn one_sided_declaration_pairs_both_slots() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
            parse_instances(r#"[{ instance_id: "arm_1", pairings: { controller: "ctrl_1" } }]"#);
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1, "agreeing pair must dedupe");
    }

    #[test]
    fn both_sides_disagreeing_is_a_conflict() {
        let arm_instances = parse_instances(
            r#"[
                { instance_id: "arm_1", pairings: { controller: "ctrl_2" } },
                { instance_id: "arm_2" }
            ]"#,
        );
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "ctrl_1", pairings: { arm: "arm_1" } },
                { instance_id: "ctrl_2" }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingConflict(_))),
            "expected PairingConflict, got {:?}",
            out.errors
        );
    }

    #[test]
    fn dead_key_is_rejected() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { ghost_slot: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        let dead = out
            .errors
            .iter()
            .find_map(|e| match e {
                ParsingError::PairingDeadKey(info) => Some(info),
                _ => None,
            })
            .expect("expected PairingDeadKey");
        assert_eq!(dead.owner_instance_id, "ctrl_1");
        assert_eq!(dead.key, "ghost_slot");
        assert_eq!(dead.declared_link_ids, "arm");
    }

    #[test]
    fn unknown_target_instance_is_rejected() {
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "ghost" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "other_ctrl" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("other_controller", &other_ctrl_instances, &other_ctrl_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
        // A dual-role node with two complementary 'arm' slots.
        let dual_arm_instances = parse_instances(r#"[{ instance_id: "dual_1" }]"#);
        let dual_arm_deps = parse_pairing_deps(
            r#"[
                { name: "arm_link", tag: "v1", role: "arm", link_id: "left_ctl", optional: true },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "right_ctl", optional: true }
            ]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "dual_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("dual_arm", &dual_arm_instances, &dual_arm_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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

        // Explicit disambiguation resolves it.
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "dual_1/left_ctl" } }]"#);
        let items = vec![
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
            item("dual_arm", &dual_arm_instances, &dual_arm_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
        assert_eq!(out.planned[0].b.link_id, "left_ctl");
    }

    #[test]
    fn in_plan_exclusivity_rejects_second_claim_on_same_slot() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances = parse_instances(
            r#"[
                { instance_id: "ctrl_1", pairings: { arm: "arm_1" } },
                { instance_id: "ctrl_2", pairings: { arm: "arm_1" } }
            ]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances = parse_instances(
            r#"[{ instance_id: "ctrl_2", pairings: { arm: "arm_1/controller" } }]"#,
        );
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            preexisting("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let already: AlreadyPairedSlots = [(
            ("arm_1".to_string(), "controller".to_string()),
            "ctrl_1:arm".to_string(),
        )]
        .into_iter()
        .collect();
        let out = validate_pairings(&items, &already, OptionalDeferPolicy::Reject);
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

    #[test]
    fn required_slot_uncovered_fails_loudly() {
        let ctrl_instances = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
            msg.contains("--pair arm@"),
            "message should show the fix: {msg}"
        );
        assert!(
            msg.contains("--defer-pair arm"),
            "message should show the defer: {msg}"
        );
    }

    #[test]
    fn optional_slot_needs_no_coverage() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps(true);
        let items = vec![item("robot_arm", &arm_instances, &arm_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
    }

    #[test]
    fn deferred_required_slot_is_covered() {
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", defer_pairings: ["arm"] }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.planned.is_empty());
    }

    #[test]
    fn defer_on_unknown_optional_or_paired_slot_is_invalid() {
        let arm_instances =
            parse_instances(r#"[{ instance_id: "arm_1", defer_pairings: ["controller"] }]"#);
        let arm_pairing_deps = arm_deps(true); // optional slot: deferring is redundant
        let items = vec![item("robot_arm", &arm_instances, &arm_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(
            out.errors
                .iter()
                .any(|e| matches!(e, ParsingError::PairingDeferInvalid { .. })),
            "expected PairingDeferInvalid for optional slot, got {:?}",
            out.errors
        );

        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", defer_pairings: ["ghost"] }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(
            out.errors.iter().any(|e| matches!(
                e,
                ParsingError::PairingDeferInvalid { link_id, .. } if link_id == "ghost"
            )),
            "expected PairingDeferInvalid for unknown slot, got {:?}",
            out.errors
        );

        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = arm_deps(true);
        let ctrl_instances = parse_instances(
            r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" }, defer_pairings: ["arm"] }]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(
            out.errors.iter().any(|e| matches!(
                e,
                ParsingError::PairingDeferInvalid { reason, .. } if reason.contains("also paired")
            )),
            "expected PairingDeferInvalid for paired+deferred slot, got {:?}",
            out.errors
        );
    }

    /// The daemon's `node_run` re-check runs under `Allow`: `stack launch`
    /// auto-defers the earlier endpoint of every planned pair — optional
    /// slots included — so a mechanism-generated optional defer must pass.
    /// Unknown and paired+deferred slots stay invalid regardless of policy.
    #[test]
    fn allow_policy_accepts_optional_defers_only() {
        let arm_instances =
            parse_instances(r#"[{ instance_id: "arm_1", defer_pairings: ["controller"] }]"#);
        let arm_pairing_deps = arm_deps(true);
        let items = vec![item("robot_arm", &arm_instances, &arm_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Allow);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);

        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", defer_pairings: ["ghost"] }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![item("arm_controller", &ctrl_instances, &ctrl_pairing_deps)];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Allow);
        assert!(
            out.errors.iter().any(|e| matches!(
                e,
                ParsingError::PairingDeferInvalid { link_id, .. } if link_id == "ghost"
            )),
            "unknown slot must stay invalid under Allow, got {:?}",
            out.errors
        );
    }

    #[test]
    fn sha256_mismatch_between_pinned_sides_is_rejected() {
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        let arm_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true, sha256: "aaa" }]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = parse_pairing_deps(
            r#"[{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm", sha256: "bbb" }]"#,
        );
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
            r#"[{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true, sha256: "aaa" }]"#,
        );
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            item("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
        let arm_pairing_deps = arm_deps(true);
        let commander_instances = parse_instances(
            r#"[{
                instance_id: "commander",
                pairings: { left_arm: "left_arm_inst", right_arm: "right_arm_inst" }
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
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
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
        // arm_1 runs already (its optional slot unpaired); ctrl_1 launches
        // with a pair naming it. ctrl_1's required slot is covered by its
        // own claim; arm_1 is never coverage-checked.
        let arm_instances = parse_instances(r#"[{ instance_id: "arm_1" }]"#);
        // Even a REQUIRED slot on the preexisting instance must not
        // trigger coverage (it was covered at its own launch).
        let arm_pairing_deps = arm_deps(false);
        let ctrl_instances =
            parse_instances(r#"[{ instance_id: "ctrl_1", pairings: { arm: "arm_1" } }]"#);
        let ctrl_pairing_deps = controller_deps();
        let items = vec![
            preexisting("robot_arm", &arm_instances, &arm_pairing_deps),
            item("arm_controller", &ctrl_instances, &ctrl_pairing_deps),
        ];
        let out = validate_pairings(&items, &BTreeMap::new(), OptionalDeferPolicy::Reject);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(out.planned.len(), 1);
    }
}
