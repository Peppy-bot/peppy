//! Cross-family validation for the unified `links` map and `defer_links` list.
//!
//! The per-mechanism validators (`bindings`, `pairings`, `observations`) each
//! own the keys that name their own slot kind and silently skip the rest. This
//! pass is the single place that sees every declared slot at once, so it owns
//! the two rules that need the union of all families:
//!   - a `links` key naming no declared slot in any family is
//!     [`ParsingError::LinkUnknownSlot`];
//!   - a `defer_links` entry that is structurally invalid (it names a
//!     producer-binding slot, which cannot be deferred, or names no slot at
//!     all) is [`ParsingError::LinkDeferInvalid`]. Stateful defer problems (an
//!     optional participant slot, or a slot that is also linked in the same
//!     plan) are judged by `pairings` / `observations`, which hold that state.
//!
//! It reuses [`BindingValidationItem`] because that item already carries the
//! full `depends_on` (all three families), so callers build no extra item type.

use crate::error::{LinkUnknownSlot, ParsingError};
use config::node::DependsOn;

use super::bindings::BindingValidationItem;

/// Run the cross-family key/defer checks over the plan. Returns aggregated
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

            for link in instance.links.keys() {
                if slots.kind_of(link).is_none() {
                    errors.push(ParsingError::LinkUnknownSlot(Box::new(LinkUnknownSlot {
                        owner_instance_id: owner_id.to_string(),
                        link: link.clone(),
                        declared_link_ids: slots.declared_csv(),
                    })));
                }
            }

            for link_id in &instance.defer_links {
                let reason = match slots.kind_of(link_id) {
                    None => Some("no such link slot is declared".to_string()),
                    Some(SlotKind::Binding) => Some(
                        "it names a producer-binding slot; only pairing or observer slots \
                         can be deferred"
                            .to_string(),
                    ),
                    // Participant / observer defers are structurally valid; any
                    // remaining problem is stateful and judged elsewhere.
                    Some(SlotKind::Participant | SlotKind::Observer) => None,
                };
                if let Some(reason) = reason {
                    errors.push(ParsingError::LinkDeferInvalid {
                        owner_instance_id: owner_id.to_string(),
                        link_id: link_id.clone(),
                        reason,
                    });
                }
            }
        }
    }

    errors
}

enum SlotKind {
    Binding,
    Participant,
    Observer,
}

/// The declared link_ids of one node, tagged by family, for O(1) kind lookup
/// and a stable declared-keys listing in error messages.
struct DeclaredLinkSlots<'a> {
    binding: Vec<&'a str>,
    participant: Vec<&'a str>,
    observer: Vec<&'a str>,
}

impl<'a> From<&'a DependsOn> for DeclaredLinkSlots<'a> {
    fn from(depends_on: &'a DependsOn) -> Self {
        let binding = depends_on
            .nodes
            .iter()
            .map(|d| d.link_id.as_str())
            .chain(depends_on.contracts.iter().map(|d| d.link_id.as_str()))
            .collect();
        let mut participant = Vec::new();
        let mut observer = Vec::new();
        for dep in &depends_on.pairings {
            if dep.is_observer() {
                observer.push(dep.link_id());
            } else {
                participant.push(dep.link_id());
            }
        }
        Self {
            binding,
            participant,
            observer,
        }
    }
}

impl DeclaredLinkSlots<'_> {
    fn kind_of(&self, link: &str) -> Option<SlotKind> {
        if self.binding.contains(&link) {
            Some(SlotKind::Binding)
        } else if self.participant.contains(&link) {
            Some(SlotKind::Participant)
        } else if self.observer.contains(&link) {
            Some(SlotKind::Observer)
        } else {
            None
        }
    }

    /// Every declared link_id across all families, sorted for a deterministic
    /// message.
    fn declared_csv(&self) -> String {
        let mut all: Vec<&str> = self
            .binding
            .iter()
            .chain(self.participant.iter())
            .chain(self.observer.iter())
            .copied()
            .collect();
        all.sort_unstable();
        all.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::DeploymentInstance;
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
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    { name: "arm_link", tag: "v1", observes_role: "arm", link_id: "watch" }
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
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    { name: "arm_link", tag: "v1", observes_role: "arm", link_id: "watch" }
                ]
            }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn deferring_a_binding_slot_is_invalid() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1" },
                defer_links: ["main"]
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{ nodes: [{ name: "camera", tag: "v1", link_id: "main" }] }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ParsingError::LinkDeferInvalid { reason, .. } if reason.contains("producer-binding")
            )),
            "expected LinkDeferInvalid for a deferred binding slot: {errors:?}"
        );
    }

    #[test]
    fn deferring_an_unknown_slot_is_invalid() {
        let instances = parse_instances(
            r#"[{ instance_id: "cons1", defer_links: ["ghost"] }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }] }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ParsingError::LinkDeferInvalid { link_id, reason, .. }
                    if link_id == "ghost" && reason.contains("no such link slot")
            )),
            "expected LinkDeferInvalid for an unknown deferred slot: {errors:?}"
        );
    }

    #[test]
    fn deferring_a_pairing_or_observer_slot_is_structurally_valid() {
        let instances = parse_instances(
            r#"[{ instance_id: "cons1", defer_links: ["arm", "watch"] }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    { name: "arm_link", tag: "v1", observes_role: "arm", link_id: "watch" }
                ]
            }"#,
        );
        let errors = validate_link_slots(&[item(&instances, Some(&depends_on))]);
        assert!(
            errors.is_empty(),
            "participant/observer defers are structurally valid here: {errors:?}"
        );
    }
}
