//! Cross-family validation for the unified `links` map.
//!
//! The per-mechanism validators (`bindings`, `pairings`, `observations`) each
//! own the keys that name their own slot kind and silently skip the rest. This
//! pass is the single place that sees every declared slot at once, so it owns
//! the two rules that need the union of all families:
//!   - a `links` key naming no declared slot in any family is
//!     [`ParsingError::LinkUnknownSlot`];
//!   - a `{ vacant: "<why>" }` value on a slot the node's own manifest does not
//!     declare emptiable is [`ParsingError::LinkVacantInvalid`]. Vacancy is a
//!     two-sided contract: the manifest says a slot MAY run empty (`optional:
//!     true` on a participant, `cardinality: "zero_or_one"` on an observer or
//!     on a producer-binding slot), and the deployment says a specific instance
//!     DOES, and why. A slot the manifest declares required cannot be vacated
//!     at all, and one that already has a spelling for "empty" (`[]`, or an
//!     omitted key on a `zero_or_more` slot) keeps using it, because one
//!     spelling per fact is the point.
//!
//! It reuses [`BindingValidationItem`] because that item already carries the
//! full `depends_on` (all three families), so callers build no extra item type.

use super::types::Placements;
use crate::error::{
    BINDING_EMPTIABLE_KEY, LinkUnknownSlot, OBSERVER_EMPTIABLE_KEY, PARTICIPANT_EMPTIABLE_KEY,
    ParsingError, VacancyRefusal,
};
use config::node::{Cardinality, DependsOn};
use std::collections::BTreeMap;

use super::bindings::{BindingValidationItem, validate_bindings};
use super::observations::{PlannedObservation, validate_observations};
use super::pairings::{
    AlreadyPairedSlots, ExternallyCoveredSlots, PairingValidationItem, PlannedPairing,
    validate_pairings,
};

/// Fully resolved output of the unified link-validation pipeline.
#[derive(Debug, Default)]
pub struct ValidatedLinkPlan {
    pub errors: Vec<ParsingError>,
    pub slot_bindings: BTreeMap<String, config::runtime::SlotBindings>,
    pub planned_pairings: Vec<PlannedPairing>,
    pub planned_observations: Vec<PlannedObservation>,
}

/// Runs cross-family validation, producer binding resolution, then pairing
/// and observation resolution in the one required order. Both stack launch
/// and CLI preflight use this entry point so a new rule cannot be wired into
/// one path but omitted from the other.
pub fn validate_link_plan(
    binding_items: &[BindingValidationItem<'_>],
    pairing_items: &[PairingValidationItem<'_>],
    already_paired: &AlreadyPairedSlots,
    externally_covered: &ExternallyCoveredSlots,
    placements: &Placements,
) -> ValidatedLinkPlan {
    let mut out = ValidatedLinkPlan {
        errors: validate_link_slots(binding_items),
        ..ValidatedLinkPlan::default()
    };
    if !out.errors.is_empty() {
        return out;
    }

    let bindings = validate_bindings(binding_items, placements);
    if !bindings.errors.is_empty() {
        out.errors = bindings.errors;
        return out;
    }
    out.slot_bindings = bindings.slot_bindings;

    let pairings = validate_pairings(pairing_items, already_paired, externally_covered);
    let observations = validate_observations(pairing_items, placements);
    out.errors.extend(pairings.errors);
    out.errors.extend(observations.errors);
    if out.errors.is_empty() {
        out.planned_pairings = pairings.planned;
        out.planned_observations = observations.planned;
    }
    out
}

/// Run the cross-family key/vacancy checks over the plan. Returns aggregated
/// errors only; the per-mechanism validators produce the resolved plans.
pub fn validate_link_slots(items: &[BindingValidationItem<'_>]) -> Vec<ParsingError> {
    let mut errors = Vec::new();

    for item in items {
        let Some(depends_on) = item.depends_on else {
            // Inert / already-running producers contribute no slots and carry
            // no links of their own; nothing to validate.
            continue;
        };
        let slots = DeclaredLinkSlots::from(depends_on);

        for instance in item.instances {
            let owner_id = instance.instance_id.as_str();

            for (link, value) in &instance.links {
                let Some(kind) = slots.kind_of(link) else {
                    errors.push(ParsingError::LinkUnknownSlot(Box::new(LinkUnknownSlot {
                        owner_instance_id: owner_id.to_string(),
                        link: link.clone(),
                        declared_link_ids: slots.declared_csv(),
                    })));
                    continue;
                };
                if value.vacancy().is_none() {
                    continue;
                }
                if let Some(refusal) = kind.refuse_vacancy() {
                    errors.push(ParsingError::LinkVacantInvalid {
                        owner_instance_id: owner_id.to_string(),
                        link_id: link.clone(),
                        slot_kind: kind.describe(),
                        refusal,
                    });
                }
            }
        }
    }

    errors
}

#[derive(Clone, Copy)]
enum LinkSlotKind {
    Binding(Cardinality),
    /// A participant pairing slot, carrying whether its manifest entry declares
    /// it `optional: true`.
    Participant {
        optional: bool,
    },
    Observer(Cardinality),
}

impl LinkSlotKind {
    /// Why this slot refuses `{ vacant: "<why>" }`, or `None` when the node's
    /// manifest declares it emptiable and vacancy is how a deployment says so.
    ///
    /// Vacancy is legal on exactly three slots, one per family, and each says
    /// so in its own manifest key: `optional: true` on a participant, and
    /// `cardinality: "zero_or_one"` on an observer or on a producer-binding
    /// slot. Every other slot is refused, with the reason that fits it. A slot
    /// the manifest declares required is not emptiable at all, so the remedy
    /// is to fill it or to change the manifest. A slot that is already
    /// emptiable through another spelling (`[]`, or an omitted `zero_or_more`
    /// key) keeps that spelling, because one spelling per fact is the point.
    fn refuse_vacancy(self) -> Option<VacancyRefusal> {
        let multi_slot_empty_set = || {
            Some(VacancyRefusal::SpelledDifferently {
                empty_spelling: "an empty array `[]`, or omitting the key entirely",
            })
        };
        match self {
            LinkSlotKind::Participant { optional: true }
            | LinkSlotKind::Observer(Cardinality::ZeroOrOne)
            | LinkSlotKind::Binding(Cardinality::ZeroOrOne) => None,
            LinkSlotKind::Participant { optional: false } => {
                Some(VacancyRefusal::ManifestRequires {
                    declare_optional: PARTICIPANT_EMPTIABLE_KEY,
                })
            }
            LinkSlotKind::Observer(Cardinality::One) => Some(VacancyRefusal::ManifestRequires {
                declare_optional: OBSERVER_EMPTIABLE_KEY,
            }),
            LinkSlotKind::Binding(Cardinality::One) => Some(VacancyRefusal::ManifestRequires {
                declare_optional: BINDING_EMPTIABLE_KEY,
            }),
            LinkSlotKind::Observer(Cardinality::ZeroOrMore)
            | LinkSlotKind::Binding(Cardinality::ZeroOrMore) => multi_slot_empty_set(),
            LinkSlotKind::Observer(Cardinality::OneOrMore) => Some(VacancyRefusal::NoEmptyState {
                requirement: "at least one source, or `cardinality: \"zero_or_more\"` on its \
                              `depends_on.pairing_observers` entry",
            }),
            LinkSlotKind::Binding(Cardinality::OneOrMore) => Some(VacancyRefusal::NoEmptyState {
                requirement: "at least one bound producer, or `cardinality: \"zero_or_more\"` on \
                              its `depends_on.nodes` / `depends_on.contracts` entry",
            }),
        }
    }

    /// The slot kind in the vocabulary its own error messages use, carrying its
    /// own article so "an observer slot" reads correctly alongside "a producer
    /// slot".
    fn describe(self) -> String {
        match self {
            LinkSlotKind::Binding(cardinality) => {
                format!("a producer-binding slot (cardinality `{cardinality}`)")
            }
            LinkSlotKind::Participant { optional: false } => {
                "a required participant pairing slot".to_string()
            }
            LinkSlotKind::Participant { optional: true } => {
                "an optional participant pairing slot".to_string()
            }
            LinkSlotKind::Observer(cardinality) => {
                format!("an observer slot (cardinality `{cardinality}`)")
            }
        }
    }
}

/// The declared link_ids of one node, tagged by family, for logarithmic lookup
/// and a stable declared-keys listing in error messages.
struct DeclaredLinkSlots<'a> {
    by_id: BTreeMap<&'a str, LinkSlotKind>,
}

impl<'a> From<&'a DependsOn> for DeclaredLinkSlots<'a> {
    fn from(depends_on: &'a DependsOn) -> Self {
        let mut by_id = BTreeMap::new();
        for (link_id, cardinality) in depends_on
            .nodes
            .iter()
            .map(|dependency| (dependency.link_id.as_str(), dependency.cardinality))
            .chain(
                depends_on
                    .contracts
                    .iter()
                    .map(|dependency| (dependency.link_id.as_str(), dependency.cardinality)),
            )
        {
            by_id.insert(link_id, LinkSlotKind::Binding(cardinality));
        }
        for dep in &depends_on.pairings {
            by_id.insert(
                dep.link_id.as_str(),
                LinkSlotKind::Participant {
                    optional: dep.optional,
                },
            );
        }
        for dep in &depends_on.pairing_observers {
            by_id.insert(
                dep.link_id.as_str(),
                LinkSlotKind::Observer(dep.cardinality),
            );
        }
        Self { by_id }
    }
}

impl DeclaredLinkSlots<'_> {
    fn kind_of(&self, link: &str) -> Option<LinkSlotKind> {
        self.by_id.get(link).copied()
    }

    /// Every declared link_id across all families, sorted for a deterministic
    /// message.
    fn declared_csv(&self) -> String {
        self.by_id.keys().copied().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::DeploymentInstance;
    use super::*;
    use config::node::ImplementsEntry;

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_depends_on(json5: &str) -> DependsOn {
        serde_json5::from_str(json5).expect("depends_on fixture should parse")
    }

    fn item<'a>(
        instances: &'a [DeploymentInstance],
        depends_on: Option<&'a DependsOn>,
    ) -> BindingValidationItem<'a> {
        BindingValidationItem {
            node_name: "cons",
            node_tag: "v1",
            instances,
            depends_on,
            implements: &[] as &[ImplementsEntry],
        }
    }

    #[test]
    fn unknown_key_lists_every_declared_slot() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1", ghost_slot: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }],
                pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
                ],
                pairing_observers: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "watch" }
                ]
            }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        let info = errors
            .iter()
            .find_map(|e| match e {
                ParsingError::LinkUnknownSlot(info) => Some(info),
                _ => None,
            })
            .expect("expected LinkUnknownSlot");
        assert_eq!(info.link, "ghost_slot");
        // All three families are listed, sorted.
        assert_eq!(info.declared_link_ids, "arm, main, watch");
    }

    #[test]
    fn known_keys_of_every_family_are_accepted() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1", arm: "arm_1", watch: "arm_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }],
                pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
                ],
                pairing_observers: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "watch" }
                ]
            }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn a_vacant_producer_slot_is_rejected_with_its_own_empty_spelling() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: {
                    main: { vacant: "no camera on this rig" },
                    extras: { vacant: "no camera on this rig" }
                }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "extras", cardinality: "zero_or_more" }
                ]
            }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(
            messages.len(),
            2,
            "both slots must be rejected: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("`main`")
                && m.contains("producer-binding slot")
                && m.contains("declares it required")
                && m.contains("`cardinality: \"zero_or_one\"`")),
            "a required producer slot names the manifest key that earns an empty state: \
             {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("`extras`") && m.contains("empty array `[]`")),
            "a zero_or_more producer slot already spells its empty set: {messages:?}"
        );
    }

    #[test]
    fn a_vacant_slot_naming_no_declared_slot_is_an_unknown_key() {
        let instances = parse_instances(
            r#"[{ instance_id: "cons1", links: { ghost: { vacant: "nothing here" } } }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }] }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ParsingError::LinkUnknownSlot(info) if info.link == "ghost"
            )),
            "a vacancy on an undeclared slot is judged as any other unknown key: {errors:?}"
        );
    }

    /// The legality table, row by row. Vacancy is legal on exactly the three
    /// slots the node's own manifest declares emptiable, one per family, and
    /// every other slot is refused with the reason that fits it: the manifest
    /// requires the slot filled, the slot has no empty state at any size, or
    /// it already has its own spelling for "empty".
    #[test]
    fn vacancy_is_legal_only_where_the_manifest_declares_the_slot_emptiable() {
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "wrist_camera", cardinality: "zero_or_one" },
                    { name: "camera", tag: "v1", link_id: "cameras", cardinality: "one_or_more" },
                    { name: "camera", tag: "v1", link_id: "spare_cameras", cardinality: "zero_or_more" }
                ],
                pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "spare_arm", optional: true }
                ],
                pairing_observers: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "watch" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "maybe_watch", cardinality: "zero_or_one" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "watched_arms", cardinality: "one_or_more" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "spare_arms", cardinality: "zero_or_more" }
                ]
            }"#,
        );
        for (link_id, expected_hint) in [
            ("spare_arm", None),
            ("maybe_watch", None),
            ("wrist_camera", None),
            ("arm", Some("`optional: true`")),
            ("watch", Some("`cardinality: \"zero_or_one\"`")),
            ("watched_arms", Some("at least one source")),
            ("spare_arms", Some("empty array `[]`")),
            ("main", Some("`cardinality: \"zero_or_one\"`")),
            ("cameras", Some("at least one bound producer")),
            ("spare_cameras", Some("empty array `[]`")),
        ] {
            let instances = parse_instances(&format!(
                r#"[{{ instance_id: "cons1", links: {{ {link_id}: {{ vacant: "why not" }} }} }}]"#
            ));
            let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
            match expected_hint {
                None => assert!(
                    errors.is_empty(),
                    "the manifest declares `{link_id}` emptiable, so vacancy is legal: {errors:?}"
                ),
                Some(hint) => {
                    let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
                    assert!(
                        messages.iter().any(|m| m.contains(hint)),
                        "`{link_id}` should be pointed at `{hint}`: {messages:?}"
                    );
                }
            }
        }
    }

    /// A required slot's refusal names the manifest, not another spelling:
    /// there is no way to write it empty in a launcher at all, so the only
    /// remedies are a peer or a manifest change.
    #[test]
    fn a_required_slot_refuses_vacancy_by_naming_the_manifest() {
        let depends_on = parse_depends_on(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }] }"#,
        );
        let instances = parse_instances(
            r#"[{ instance_id: "cons1", links: { arm: { vacant: "no commander on this rig" } } }]"#,
        );
        let messages: Vec<String> = validate_link_slots(&[item(&instances, Some(&depends_on))])
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(messages.len(), 1, "one slot, one refusal: {messages:?}");
        let message = &messages[0];
        assert!(
            message.contains("required participant pairing slot")
                && message.contains("declares it required")
                && message.contains("`optional: true`"),
            "the refusal must name the manifest and the key that lifts it: {message}"
        );
        assert!(
            !message.contains("empty array"),
            "a required slot has no empty spelling to be pointed at: {message}"
        );
    }
}
