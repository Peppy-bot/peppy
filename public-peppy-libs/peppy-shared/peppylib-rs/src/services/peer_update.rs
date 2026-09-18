//! Framework `peer_update` service: the daemon's live delivery channel for
//! pairing-slot state (the pairs a slot holds). Registered pre-setup: user
//! code may block in `setup_fn` forever, and pairing delivery must not depend
//! on it. The sequenced, daemon-only, idempotent delivery protocol lives in
//! [`crate::services::slot_update`]; this module only maps a
//! `PeerUpdateRequest` onto a pairing slot's [`PeerSetState`].
//!
//! Pairing state is daemon-authoritative: stacks are daemon-scoped, so the
//! only legitimate caller is the node's own daemon, whose identity the node
//! knows as its bound core_node. (Identity stamps are cooperative on the fabric
//! — this guards against misdirected or misbehaving callers; transport-level
//! access control remains the security boundary.)

use crate::encoding::peer_update::PeerUpdateRequest;
use crate::messaging::{PEER_UPDATE_SERVICE, PeerSetState, SenderTarget};
use crate::runtime::TaskHandle;
use crate::services::slot_update::{SlotSenders, SlotUpdate, listen_for_slot_update};
use crate::{MessengerHandle, PeppyResult};

/// Shared map of one watch channel per declared pairing slot, keyed by the
/// node's own slot link_id.
pub(crate) type PairingSlotSenders = SlotSenders<PeerSetState>;

impl SlotUpdate for PeerUpdateRequest {
    type State = PeerSetState;

    const SERVICE: &'static str = PEER_UPDATE_SERVICE;
    const UNKNOWN_SLOT_NOUN: &'static str = "pairing slot";

    fn decode_request(payload: &[u8]) -> PeppyResult<Self> {
        PeerUpdateRequest::decode(payload)
    }

    fn link_id(&self) -> &str {
        &self.link_id
    }

    fn sequence(&self) -> u64 {
        self.sequence
    }

    fn state_sequence(state: &PeerSetState) -> u64 {
        state.sequence
    }

    /// Replace-wholesale: a delivery carries every pair the slot holds, so
    /// pairs it omits are gone from the slot and its order is the order the
    /// slot holds.
    fn to_state(&self) -> PeerSetState {
        PeerSetState {
            sequence: self.sequence,
            members: self.members.clone(),
        }
    }

    fn log_detail(&self) -> String {
        format!("members={}", self.members.len())
    }
}

pub async fn listen_for_peer_update(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    slots: PairingSlotSenders,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    listen_for_slot_update::<PeerUpdateRequest>(
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
    use crate::messaging::{PeerInfo, PeerMember, ProducerRef};
    use crate::services::slot_update::apply_slot_update;
    use std::collections::BTreeMap;
    use tokio::sync::watch;

    fn apply(
        slots: &BTreeMap<String, watch::Sender<PeerSetState>>,
        request: &PeerUpdateRequest,
    ) -> SlotUpdateResponse {
        apply_slot_update::<PeerUpdateRequest>(slots, request)
    }

    fn slot_map(link_ids: &[&str]) -> BTreeMap<String, watch::Sender<PeerSetState>> {
        link_ids
            .iter()
            .map(|id| {
                let (tx, _rx) = watch::channel(PeerSetState::empty());
                (id.to_string(), tx)
            })
            .collect()
    }

    fn pin(core: &str, inst: &str, peer_link: &str) -> PeerMember {
        PeerMember {
            info: PeerInfo {
                producer: ProducerRef::new(core, inst),
                peer_link_id: peer_link.to_string(),
            },
            copy: None,
        }
    }

    fn request(link_id: &str, sequence: u64, pin: Option<PeerMember>) -> PeerUpdateRequest {
        PeerUpdateRequest {
            link_id: link_id.to_string(),
            sequence,
            members: pin.into_iter().collect(),
        }
    }

    fn held(watched: &watch::Receiver<PeerSetState>) -> Option<PeerMember> {
        watched.borrow().members.first().cloned()
    }

    #[test]
    fn applies_pair_then_clear() {
        let slots = slot_map(&["arm"]);
        let watched = slots["arm"].subscribe();

        let paired = apply(
            &slots,
            &request("arm", 10, Some(pin("core_a", "arm_1", "controller"))),
        );
        assert!(paired.accepted);
        assert_eq!(held(&watched), Some(pin("core_a", "arm_1", "controller")));

        let cleared = apply(&slots, &request("arm", 11, None));
        assert!(cleared.accepted);
        assert_eq!(held(&watched), None);
        assert_eq!(watched.borrow().sequence, 11);
    }

    #[test]
    fn rejects_strictly_stale_sequence_without_rollback() {
        let slots = slot_map(&["arm"]);
        let watched = slots["arm"].subscribe();

        apply(
            &slots,
            &request("arm", 20, Some(pin("core_a", "arm_2", "controller"))),
        );
        // A delayed earlier delivery arrives after the newer one.
        let response = apply(
            &slots,
            &request("arm", 19, Some(pin("core_a", "arm_1", "controller"))),
        );
        assert!(!response.accepted);
        assert!(response.stale_sequence);
        assert_eq!(
            held(&watched),
            Some(pin("core_a", "arm_2", "controller")),
            "stale request must not roll the slot back"
        );
    }

    #[test]
    fn equal_sequence_retry_is_idempotent_and_accepted() {
        let slots = slot_map(&["arm"]);
        let mut watched = slots["arm"].subscribe();

        apply(
            &slots,
            &request("arm", 5, Some(pin("core_a", "arm_1", "controller"))),
        );
        assert!(watched.has_changed().unwrap());
        watched.mark_unchanged();

        let retry = apply(
            &slots,
            &request("arm", 5, Some(pin("core_a", "arm_1", "controller"))),
        );
        assert!(retry.accepted);
        assert!(
            !watched.has_changed().unwrap(),
            "an identical retry must not re-notify watchers"
        );
    }

    /// One sequence carries one answer. A second delivery reusing it with a
    /// different set is refused, so the slot keeps agreeing with the daemon
    /// that sent the first.
    #[test]
    fn equal_sequence_asserting_different_members_is_refused() {
        let slots = slot_map(&["arm"]);
        let watched = slots["arm"].subscribe();

        apply(
            &slots,
            &request("arm", 7, Some(pin("core_a", "arm_1", "controller"))),
        );
        let response = apply(
            &slots,
            &request("arm", 7, Some(pin("core_a", "arm_2", "controller"))),
        );
        assert!(!response.accepted);
        assert!(!response.stale_sequence);
        assert_eq!(
            held(&watched),
            Some(pin("core_a", "arm_1", "controller")),
            "a conflicting retry must not move the slot"
        );
    }

    #[test]
    fn unknown_slot_is_rejected() {
        let slots = slot_map(&["arm"]);
        let response = apply(
            &slots,
            &request("gripper", 1, Some(pin("core_a", "g_1", "controller"))),
        );
        assert!(!response.accepted);
        assert!(!response.stale_sequence);
        assert!(response.message.contains("gripper"));
    }

    #[test]
    fn a_delivery_replaces_the_whole_set_in_its_order() {
        let slots = slot_map(&["arms"]);
        let watched = slots["arms"].subscribe();
        let two = PeerUpdateRequest {
            link_id: "arms".to_string(),
            sequence: 3,
            members: vec![
                pin("core_a", "arm_2", "controller"),
                pin("core_b", "arm_1", "controller"),
            ],
        };
        assert!(apply(&slots, &two).accepted);
        assert_eq!(watched.borrow().members, two.members);

        let one = PeerUpdateRequest {
            link_id: "arms".to_string(),
            sequence: 4,
            members: vec![pin("core_b", "arm_1", "controller")],
        };
        assert!(apply(&slots, &one).accepted);
        assert_eq!(
            watched.borrow().members,
            one.members,
            "a pair the delivery omits is gone from the slot"
        );
    }
}
