//! Framework `observation_update` service: the daemon's live delivery channel
//! for observer-slot state (source pin, source generation, source liveness).
//! Registered pre-setup — user code may block in `setup_fn` forever, and
//! observation delivery must not depend on it. The sequenced, daemon-only,
//! idempotent delivery protocol lives in [`crate::services::slot_update`]; this
//! module only maps an `ObservationUpdateRequest` onto an observer slot's
//! [`ObservationState`].
//!
//! Observation state is daemon-authoritative, so the only legitimate caller is
//! the node's own daemon, whose identity the node knows as its bound core_node.

use crate::encoding::observation_update::ObservationUpdateRequest;
use crate::messaging::{OBSERVATION_UPDATE_SERVICE, ObservationState, SenderTarget};
use crate::runtime::TaskHandle;
use crate::services::slot_update::{SlotChannels, SlotUpdate, listen_for_slot_update};
use crate::{MessengerHandle, PeppyResult};
use config::node::Cardinality;

/// Shared map of one watch channel per declared observer slot, keyed by the
/// node's own observer-slot link_id.
pub(crate) type ObservationSlotChannels = SlotChannels<ObservationState>;

impl SlotUpdate for ObservationUpdateRequest {
    type State = ObservationState;

    const SERVICE: &'static str = OBSERVATION_UPDATE_SERVICE;
    const SLOT_NOUN: &'static str = "observer slot";

    fn decode_request(payload: &[u8]) -> PeppyResult<Self> {
        ObservationUpdateRequest::decode(payload)
    }

    fn link_id(&self) -> &str {
        &self.link_id
    }

    fn sequence(&self) -> u64 {
        self.sequence
    }

    fn state_sequence(state: &ObservationState) -> u64 {
        state.sequence
    }

    /// Replace-wholesale: a delivery carries the slot's complete member set, so
    /// members it omits are gone from the slot and the plan's order is the
    /// order the slot holds.
    fn to_state(&self) -> ObservationState {
        ObservationState {
            sequence: self.sequence,
            members: self.members.clone(),
        }
    }

    fn member_count(state: &ObservationState) -> usize {
        state.members.len()
    }

    /// An observer set's membership is the plan's: the launch links it, a join
    /// grows it and a removal shrinks it, each within the slot's cardinality.
    /// A member whose source is down stays in the set, reading not live, so
    /// liveness never changes the count.
    fn admits(cardinality: Cardinality, members: usize) -> bool {
        cardinality.admits(members)
    }

    fn log_detail(&self) -> String {
        format!(
            "members={} live={}",
            self.members.len(),
            self.members
                .iter()
                .filter(|member| member.source_live)
                .count()
        )
    }
}

pub async fn listen_for_observation_update(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    slots: ObservationSlotChannels,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    listen_for_slot_update::<ObservationUpdateRequest>(
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
    use crate::messaging::{ObservedMemberState, ObservedSource, ProducerRef};
    use crate::services::slot_update::{SlotChannel, apply_slot_update};
    use std::collections::BTreeMap;

    fn apply(
        slots: &BTreeMap<String, SlotChannel<ObservationState>>,
        request: &ObservationUpdateRequest,
    ) -> SlotUpdateResponse {
        apply_slot_update::<ObservationUpdateRequest>(slots, request)
    }

    fn slot_map(link_ids: &[&str]) -> BTreeMap<String, SlotChannel<ObservationState>> {
        slot_map_of(Cardinality::ZeroOrMore, link_ids)
    }

    fn slot_map_of(
        cardinality: Cardinality,
        link_ids: &[&str],
    ) -> BTreeMap<String, SlotChannel<ObservationState>> {
        super::super::slot_update::slot_map(cardinality, link_ids, ObservationState::unregistered)
    }

    fn member(instance: &str, generation: u64, live: bool) -> ObservedMemberState {
        ObservedMemberState {
            source: ObservedSource {
                producer: ProducerRef::new("core_a", instance),
                source_link_id: "commander".to_string(),
                peer: None,
            },
            source_generation: generation,
            source_live: live,
        }
    }

    fn request(
        link_id: &str,
        sequence: u64,
        members: Vec<ObservedMemberState>,
    ) -> ObservationUpdateRequest {
        ObservationUpdateRequest {
            link_id: link_id.to_string(),
            sequence,
            members,
        }
    }

    fn instances(state: &ObservationState) -> Vec<String> {
        state
            .members
            .iter()
            .map(|m| m.source.producer.instance_id.clone())
            .collect()
    }

    #[test]
    fn applies_members_then_advances_one_generation_in_place() {
        let slots = slot_map(&["observed_arm"]);
        let watched = slots["observed_arm"].sender().subscribe();

        let first = apply(
            &slots,
            &request(
                "observed_arm",
                10,
                vec![member("arm_2", 5, true), member("arm_1", 5, true)],
            ),
        );
        assert!(first.accepted);
        assert_eq!(
            instances(&watched.borrow()),
            ["arm_2", "arm_1"],
            "the delivery's order is the slot's order"
        );

        // One member restarting advances only its own generation, at its own
        // position.
        let second = apply(
            &slots,
            &request(
                "observed_arm",
                11,
                vec![member("arm_2", 5, true), member("arm_1", 6, true)],
            ),
        );
        assert!(second.accepted);
        let state = watched.borrow().clone();
        assert_eq!(instances(&state), ["arm_2", "arm_1"]);
        assert_eq!(state.members[0].source_generation, 5);
        assert_eq!(state.members[1].source_generation, 6);
    }

    /// A delivery is the whole set, so a member it omits leaves the slot and a
    /// member it adds joins it. Nothing merges member-by-member.
    #[test]
    fn a_delivery_replaces_the_member_set_wholesale() {
        let slots = slot_map(&["observed_arm"]);
        let watched = slots["observed_arm"].sender().subscribe();

        apply(
            &slots,
            &request(
                "observed_arm",
                1,
                vec![member("arm_1", 1, true), member("arm_2", 1, true)],
            ),
        );
        apply(
            &slots,
            &request("observed_arm", 2, vec![member("arm_2", 1, true)]),
        );
        assert_eq!(instances(&watched.borrow()), ["arm_2"]);

        apply(&slots, &request("observed_arm", 3, Vec::new()));
        assert!(
            watched.borrow().members.is_empty(),
            "an empty delivery empties the slot"
        );
    }

    /// A source going down keeps its member listed at its position; only the
    /// liveness flag moves.
    #[test]
    fn a_down_source_stays_listed_with_liveness_cleared() {
        let slots = slot_map(&["observed_arm"]);
        let watched = slots["observed_arm"].sender().subscribe();

        apply(
            &slots,
            &request(
                "observed_arm",
                1,
                vec![member("arm_1", 1, true), member("arm_2", 1, true)],
            ),
        );
        apply(
            &slots,
            &request(
                "observed_arm",
                2,
                vec![member("arm_1", 1, true), member("arm_2", 1, false)],
            ),
        );
        let state = watched.borrow().clone();
        assert_eq!(instances(&state), ["arm_1", "arm_2"]);
        assert!(state.members[0].source_live);
        assert!(!state.members[1].source_live);
    }

    #[test]
    fn rejects_strictly_stale_sequence_without_rollback() {
        let slots = slot_map(&["observed_arm"]);
        let watched = slots["observed_arm"].sender().subscribe();

        apply(
            &slots,
            &request("observed_arm", 20, vec![member("arm_1", 5, true)]),
        );
        // A delayed earlier delivery arrives after the newer one.
        let response = apply(
            &slots,
            &request("observed_arm", 19, vec![member("arm_1", 4, false)]),
        );
        assert!(!response.accepted);
        assert!(response.stale_sequence);
        assert_eq!(
            watched.borrow().members[0].source_generation,
            5,
            "stale request must not roll the slot back"
        );
    }

    #[test]
    fn equal_sequence_retry_is_idempotent_and_accepted() {
        let slots = slot_map(&["observed_arm"]);
        let mut watched = slots["observed_arm"].sender().subscribe();

        apply(
            &slots,
            &request("observed_arm", 5, vec![member("arm_1", 1, true)]),
        );
        assert!(watched.has_changed().unwrap());
        watched.mark_unchanged();

        let retry = apply(
            &slots,
            &request("observed_arm", 5, vec![member("arm_1", 1, true)]),
        );
        assert!(retry.accepted);
        assert!(
            !watched.has_changed().unwrap(),
            "an identical retry must not re-notify watchers"
        );
    }

    #[test]
    fn unknown_slot_is_rejected() {
        let slots = slot_map(&["observed_arm"]);
        let response = apply(
            &slots,
            &request("observed_gripper", 1, vec![member("g_1", 1, true)]),
        );
        assert!(!response.accepted);
        assert!(!response.stale_sequence);
        assert!(response.message.contains("observed_gripper"));
    }

    /// A member whose source is down stays listed, so an empty delivery to a
    /// `one_or_more` observer slot is a miscount: it is refused, the slot
    /// keeps what it observes, and no watcher hears of it.
    #[test]
    fn a_delivery_emptying_a_one_or_more_slot_is_refused() {
        let slots = slot_map_of(Cardinality::OneOrMore, &["observed_arm"]);
        apply(
            &slots,
            &request("observed_arm", 1, vec![member("arm_1", 1, true)]),
        );
        let mut watched = slots["observed_arm"].sender().subscribe();
        watched.mark_unchanged();

        let response = apply(&slots, &request("observed_arm", 2, Vec::new()));
        assert!(!response.accepted, "{response:?}");
        assert!(
            response
                .message
                .contains("`one_or_more` admits no set of 0 members; the plan and this node's manifest disagree"),
            "{response:?}"
        );
        assert!(
            !watched.has_changed().unwrap(),
            "a refusal wakes no watcher"
        );
        assert_eq!(
            instances(&slots["observed_arm"].sender().borrow()),
            ["arm_1"],
            "the refused set never landed"
        );
    }
}
