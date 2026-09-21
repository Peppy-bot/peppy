//! Framework `binding_update` service: the daemon's live delivery channel for a
//! producer set slot's producers. Registered pre-setup, like `peer_update` and
//! `observation_update`: user code may block in `setup_fn` forever, and binding
//! delivery must not depend on it. The sequenced, daemon-only, idempotent
//! delivery protocol lives in [`crate::services::slot_update`]; this module maps
//! a `BindingUpdateRequest` onto a producer set slot's [`BoundSetState`].
//!
//! The service receives the channels of the set slots (`one_or_more`,
//! `zero_or_more`) alone, so an update naming a `one` or `zero_or_one` slot is
//! refused as an unknown slot, and a scalar slot holds the set its deployment
//! bound at launch for the node's lifetime.

use crate::encoding::binding_update::BindingUpdateRequest;
use crate::messaging::{BINDING_UPDATE_SERVICE, BoundSetState, SenderTarget};
use crate::runtime::TaskHandle;
use crate::services::slot_update::{SlotChannels, SlotUpdate, listen_for_slot_update};
use crate::{MessengerHandle, PeppyResult};
use config::node::Cardinality;

/// Shared map of one watch channel per declared producer set slot, keyed by
/// the node's own consumer-slot link_id.
pub(crate) type BindingSlotChannels = SlotChannels<BoundSetState>;

impl SlotUpdate for BindingUpdateRequest {
    type State = BoundSetState;

    const SERVICE: &'static str = BINDING_UPDATE_SERVICE;
    const SLOT_NOUN: &'static str = "producer set slot";

    fn decode_request(payload: &[u8]) -> PeppyResult<Self> {
        BindingUpdateRequest::decode(payload)
    }

    fn link_id(&self) -> &str {
        &self.link_id
    }

    fn sequence(&self) -> u64 {
        self.sequence
    }

    fn state_sequence(state: &BoundSetState) -> u64 {
        state.sequence
    }

    /// Replace-wholesale: a delivery carries every producer the slot holds, so
    /// producers it omits are gone from the slot and its order is the order the
    /// slot holds.
    fn to_state(&self) -> BoundSetState {
        BoundSetState {
            sequence: self.sequence,
            producers: self.producers.clone(),
        }
    }

    fn member_count(state: &BoundSetState) -> usize {
        state.producers.len()
    }

    /// A producer set's membership is the plan's: the launch binds it, a join
    /// grows it and a removal shrinks it, each within the slot's cardinality.
    fn admits(cardinality: Cardinality, members: usize) -> bool {
        cardinality.admits(members)
    }

    fn log_detail(&self) -> String {
        format!("producers={}", self.producers.len())
    }
}

pub async fn listen_for_binding_update(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    slots: BindingSlotChannels,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    listen_for_slot_update::<BindingUpdateRequest>(
        messenger,
        core_node,
        instance_id,
        as_identity,
        slots,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::slot_update::SlotUpdateResponse;
    use crate::services::slot_update::{SlotChannel, apply_slot_update, slot_map};
    use config::runtime::{BoundProducers, ProducerRef};
    use std::collections::BTreeMap;

    fn producers(ids: &[&str]) -> BoundProducers {
        BoundProducers::try_from(
            ids.iter()
                .map(|id| ProducerRef::new("core_a", *id))
                .collect::<Vec<_>>(),
        )
        .expect("distinct producers")
    }

    fn cameras_slot(ids: &[&str]) -> BTreeMap<String, SlotChannel<BoundSetState>> {
        cameras_slot_of(Cardinality::ZeroOrMore, ids)
    }

    fn cameras_slot_of(
        cardinality: Cardinality,
        ids: &[&str],
    ) -> BTreeMap<String, SlotChannel<BoundSetState>> {
        slot_map(cardinality, &["cameras"], || {
            BoundSetState::seeded(producers(ids))
        })
    }

    fn update(link_id: &str, sequence: u64, ids: &[&str]) -> BindingUpdateRequest {
        BindingUpdateRequest {
            link_id: link_id.to_string(),
            sequence,
            producers: producers(ids),
        }
    }

    fn held(slots: &BTreeMap<String, SlotChannel<BoundSetState>>) -> Vec<String> {
        slots["cameras"]
            .sender()
            .borrow()
            .producers
            .producers()
            .map(|producer| producer.instance_id.clone())
            .collect()
    }

    #[test]
    fn a_delivery_replaces_the_set_and_a_stale_one_leaves_it() {
        let slots = cameras_slot(&[]);
        assert_eq!(
            apply_slot_update(&slots, &update("cameras", 5, &["front", "rear"])),
            SlotUpdateResponse::accepted()
        );
        assert_eq!(held(&slots), ["front", "rear"]);
        assert_eq!(
            apply_slot_update(&slots, &update("cameras", 3, &["front"])),
            SlotUpdateResponse::stale()
        );
        assert_eq!(held(&slots), ["front", "rear"]);
        assert_eq!(
            apply_slot_update(&slots, &update("cameras", 6, &["rear"])),
            SlotUpdateResponse::accepted()
        );
        assert_eq!(held(&slots), ["rear"]);
    }

    /// The slot's floor holds at the wire: a delivery that would leave a
    /// `one_or_more` slot empty is refused, so the slot's generated accessor
    /// keeps its "at least one" guarantee, and no watcher hears of it.
    #[test]
    fn a_delivery_emptying_a_one_or_more_slot_is_refused() {
        let slots = cameras_slot_of(Cardinality::OneOrMore, &["front"]);
        let mut watched = slots["cameras"].sender().subscribe();
        watched.mark_unchanged();
        let response = apply_slot_update(&slots, &update("cameras", 9, &[]));
        assert!(!response.accepted, "{response:?}");
        assert!(
            response
                .message
                .contains("`one_or_more` admits no set of 0 members; the plan and this node's manifest disagree"),
            "{response:?}"
        );
        assert_eq!(held(&slots), ["front"], "the refused set never landed");
        assert!(
            !watched.has_changed().unwrap(),
            "a refusal wakes no watcher"
        );
    }

    #[test]
    fn an_update_naming_a_slot_the_service_does_not_hold_is_refused() {
        let slots = cameras_slot(&["front"]);
        let response = apply_slot_update(&slots, &update("main", 1, &["rear"]));
        assert!(!response.accepted, "{response:?}");
        assert!(
            response
                .message
                .contains("this node holds no producer set slot `main`"),
            "{response:?}"
        );
        assert_eq!(held(&slots), ["front"]);
    }
}
