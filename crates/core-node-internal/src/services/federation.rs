//! Participant side of a federated launch: which launch this daemon is
//! committed to, and who is driving it.
//!
//! # Why a daemon records this at all
//!
//! A federated stack spans several machines, but there is deliberately no
//! authority above the per-daemon ones that already exist. Each participant
//! owns its slice, exactly as it already owns its `NodeStack`, its pairing
//! registry, and its observation coordinator. What this module adds is the one
//! missing fact: WHICH launch that slice belongs to.
//!
//! Recording `(coordinator, launch id)` on the slice is what buys
//! discovery-instead-of-persistence. `stack list` already enumerates every live
//! core node and queries each one, so a coordinator rediscovers its
//! participants with a fan-out the CLI already performs. A restarted
//! coordinator therefore finds its own launch again, `stack reset` works from
//! any machine in the federation, and no state file is introduced anywhere.
//!
//! # Why the reservation is a lease
//!
//! Preflight RESERVES every participant before anything is torn down, rather
//! than merely asking whether each is busy. Asking is a time-of-check /
//! time-of-use race: two coordinators can both observe idle, both begin
//! dispatching, and the loser only discovers the conflict partway through,
//! after machines have already had their stacks replaced.
//!
//! But a reservation that outlives its coordinator is its own failure: a
//! coordinator that dies mid-launch would wedge every machine it had reserved
//! until each daemon restarted, with nothing in the UI to explain why. So a
//! reservation is a LEASE held against the coordinator's presence. The daemon
//! watches the coordinator's core-node presence token for as long as it holds
//! the reservation, and [`SliceOwnership::release_because_coordinator_gone`]
//! drops it the moment that token disappears.
//!
//! The registry here is deliberately pure: it owns the state machine and
//! nothing else, so every transition (including the coordinator-gone one) is
//! unit-testable without a messenger, a timer, or a sleep. Wiring the presence
//! watch to it lives in the service handler below.

mod service;

pub(crate) use service::{
    FederationServiceContext, listen_for_pair_commit, listen_for_participant_release,
    listen_for_participant_reserve, listen_for_participant_slice_begin,
    listen_for_relationship_notify,
};

use core_node_api::encoding::LaunchIdentity;
use parking_lot::Mutex;
use std::sync::Arc;

/// A reservation currently held by this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldReservation {
    launch_id: String,
    coordinator_core_node: String,
}

#[derive(Debug, Default)]
struct OwnershipState {
    /// Held only between a coordinator's preflight and the end of its launch.
    reservation: Option<HeldReservation>,
    /// The launch this daemon's current stack slice came from. Outlives the
    /// reservation: the reservation guards the launch, this describes its
    /// result.
    slice: Option<LaunchIdentity>,
}

/// Outcome of [`SliceOwnership::try_reserve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// The reservation is now held for the requesting launch. Also returned
    /// when the same launch reserves twice, so a coordinator retrying a
    /// dropped reply does not refuse itself.
    Reserved,
    /// Another launch holds it. The coordinator that receives this releases
    /// every reservation it did obtain and fails the launch, so no machine is
    /// left half-replaced.
    HeldByAnotherLaunch {
        launch_id: String,
        coordinator_core_node: String,
    },
}

/// Which launch this daemon is committed to, and which launch its current
/// stack slice came from. See the module docs for why both live here.
#[derive(Debug, Default)]
pub struct SliceOwnership {
    state: Mutex<OwnershipState>,
}

impl SliceOwnership {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reserves this daemon for `launch_id`, driven by `coordinator`.
    ///
    /// Re-reserving for the same launch succeeds: the exchange is a network
    /// round trip, so a coordinator whose reply was lost must be able to retry
    /// without deadlocking against its own reservation.
    pub fn try_reserve(&self, launch_id: &str, coordinator: &str) -> ReserveOutcome {
        let mut state = self.state.lock();
        match &state.reservation {
            Some(held) if held.launch_id == launch_id => ReserveOutcome::Reserved,
            Some(held) => ReserveOutcome::HeldByAnotherLaunch {
                launch_id: held.launch_id.clone(),
                coordinator_core_node: held.coordinator_core_node.clone(),
            },
            None => {
                state.reservation = Some(HeldReservation {
                    launch_id: launch_id.to_owned(),
                    coordinator_core_node: coordinator.to_owned(),
                });
                ReserveOutcome::Reserved
            }
        }
    }

    /// Releases a reservation held for `launch_id`.
    ///
    /// Returns `false` only when a DIFFERENT launch holds it, which the caller
    /// has no standing to release. Releasing when nothing is held succeeds: a
    /// coordinator unwinding a failed preflight cannot always tell which
    /// participants actually acked, so the release has to be idempotent.
    pub fn release(&self, launch_id: &str) -> bool {
        let mut state = self.state.lock();
        match &state.reservation {
            Some(held) if held.launch_id != launch_id => false,
            Some(_) => {
                state.reservation = None;
                true
            }
            None => true,
        }
    }

    /// Drops the reservation because the coordinator holding it vanished from
    /// the federation. This is what keeps a dead coordinator from wedging a
    /// machine until its next daemon restart.
    ///
    /// Scoped to the named coordinator so a stale watch (one belonging to an
    /// earlier, already-released reservation) cannot free a reservation that a
    /// different coordinator has since taken.
    ///
    /// Returns the launch that was released, if any.
    pub fn release_because_coordinator_gone(&self, coordinator: &str) -> Option<String> {
        let mut state = self.state.lock();
        let held = state.reservation.as_ref()?;
        if held.coordinator_core_node != coordinator {
            return None;
        }
        let launch_id = held.launch_id.clone();
        state.reservation = None;
        Some(launch_id)
    }

    /// The refusal a node action owes its caller when this machine is
    /// committed to a federated launch other than the one asking.
    ///
    /// Every node action shares it so the three cannot explain the same
    /// situation three different ways. `launch_id` is the goal's own: `None`
    /// for anything a user typed, `Some` for a coordinator's dispatch.
    ///
    /// The reservation covers the WHOLE machine, so local `node add` / `node
    /// run` consult this too. Without that, a federated launch would only
    /// exclude other launches, and a local `peppy node run` could still race
    /// it: the per-action gates are per-action, and a coordinator dispatching
    /// to a peer goes through the node actions rather than the launch one.
    ///
    /// Decided under one lock: the launch that holds the daemon and the
    /// coordinator driving it are two halves of one answer, and reading them
    /// separately would let the reservation change between the two.
    pub fn refuse_if_reserved_elsewhere(
        &self,
        launch_id: Option<&str>,
    ) -> std::result::Result<(), String> {
        let state = self.state.lock();
        let Some(held) = state.reservation.as_ref() else {
            return Ok(());
        };
        if launch_id == Some(held.launch_id.as_str()) {
            return Ok(());
        }
        Err(format!(
            "this daemon is reserved for federated launch `{}`, driven by `{}`, \
             which is replacing its whole stack. Wait for that launch to finish, or clear the \
             reservation with `peppy stack reset`.",
            held.launch_id, held.coordinator_core_node
        ))
    }

    /// Records that this daemon's stack slice came from `launch`. Called when a
    /// federated launch finishes populating this participant.
    pub fn record_slice(&self, launch: LaunchIdentity) {
        self.state.lock().slice = Some(launch);
    }

    /// The launch this daemon's slice belongs to, reported on every
    /// `stack_list` response so the slice is self-describing.
    pub fn slice(&self) -> Option<LaunchIdentity> {
        self.state.lock().slice.clone()
    }

    /// The reservation currently held, as `(launch_id, coordinator)`.
    pub fn held_reservation(&self) -> Option<(String, String)> {
        let state = self.state.lock();
        state
            .reservation
            .as_ref()
            .map(|held| (held.launch_id.clone(), held.coordinator_core_node.clone()))
    }

    /// Clears both the reservation and the slice record. Called by
    /// `stack reset`: an emptied stack belongs to no launch, and a reset also
    /// releases whatever launch was mid-flight over this machine.
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.reservation = None;
        state.slice = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_daemon_accepts_a_reservation() {
        let ownership = SliceOwnership::new();
        assert_eq!(
            ownership.try_reserve("launch-a", "cn-robot-7"),
            ReserveOutcome::Reserved
        );
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-a".to_owned(), "cn-robot-7".to_owned()))
        );
    }

    /// The race the reservation exists to close: the second coordinator is
    /// refused BEFORE it has torn anything down.
    #[test]
    fn a_second_coordinator_is_refused_and_told_who_holds_it() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");

        assert_eq!(
            ownership.try_reserve("launch-b", "cn-robot-9"),
            ReserveOutcome::HeldByAnotherLaunch {
                launch_id: "launch-a".to_owned(),
                coordinator_core_node: "cn-robot-7".to_owned(),
            }
        );
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-a".to_owned(), "cn-robot-7".to_owned())),
            "a refused reservation must not disturb the held one"
        );
    }

    /// The exchange is a network round trip, so a coordinator whose reply was
    /// lost has to be able to retry without deadlocking against itself.
    #[test]
    fn re_reserving_the_same_launch_succeeds() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        assert_eq!(
            ownership.try_reserve("launch-a", "cn-robot-7"),
            ReserveOutcome::Reserved
        );
    }

    #[test]
    fn releasing_frees_the_daemon_for_the_next_launch() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        assert!(ownership.release("launch-a"));
        assert_eq!(ownership.held_reservation(), None);
        assert_eq!(
            ownership.try_reserve("launch-b", "cn-robot-9"),
            ReserveOutcome::Reserved
        );
    }

    /// A coordinator unwinding a failed preflight cannot always tell which
    /// participants actually acked, so releasing nothing must succeed.
    #[test]
    fn releasing_an_unheld_reservation_succeeds() {
        let ownership = SliceOwnership::new();
        assert!(ownership.release("launch-a"));
    }

    #[test]
    fn releasing_another_launchs_reservation_is_refused() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        assert!(!ownership.release("launch-b"));
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-a".to_owned(), "cn-robot-7".to_owned()))
        );
    }

    /// The lease. Without this a coordinator that died mid-launch would wedge
    /// every machine it had reserved until each daemon restarted.
    #[test]
    fn a_vanished_coordinator_releases_its_reservation() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");

        assert_eq!(
            ownership.release_because_coordinator_gone("cn-robot-7"),
            Some("launch-a".to_owned())
        );
        assert_eq!(ownership.held_reservation(), None);
        assert_eq!(
            ownership.try_reserve("launch-b", "cn-robot-9"),
            ReserveOutcome::Reserved,
            "the machine must be usable again once its coordinator is gone"
        );
    }

    /// A watch belonging to an already-released reservation must not free a
    /// reservation a different coordinator has since taken.
    #[test]
    fn a_stale_coordinator_watch_does_not_release_someone_elses_reservation() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        ownership.release("launch-a");
        ownership.try_reserve("launch-b", "cn-robot-9");

        assert_eq!(
            ownership.release_because_coordinator_gone("cn-robot-7"),
            None
        );
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-b".to_owned(), "cn-robot-9".to_owned()))
        );
    }

    #[test]
    fn a_vanished_coordinator_with_no_reservation_releases_nothing() {
        let ownership = SliceOwnership::new();
        assert_eq!(
            ownership.release_because_coordinator_gone("cn-robot-7"),
            None
        );
    }

    /// The reservation covers the whole machine, not just the launch action:
    /// a coordinator dispatching to a peer drives the NODE actions, which have
    /// their own gates, so local node work has to consult this to stay out of
    /// the way.
    #[test]
    fn local_work_is_excluded_while_another_launch_holds_the_daemon() {
        let ownership = SliceOwnership::new();
        assert!(ownership.refuse_if_reserved_elsewhere(None).is_ok());

        ownership.try_reserve("launch-a", "cn-robot-7");

        let refusal = ownership
            .refuse_if_reserved_elsewhere(None)
            .expect_err("a local action names no launch, so it is excluded");
        assert!(refusal.contains("launch-a"), "got: {refusal}");
        assert!(
            refusal.contains("cn-robot-7"),
            "the refusal must name the coordinator to wait on; got: {refusal}"
        );

        assert!(
            ownership
                .refuse_if_reserved_elsewhere(Some("launch-a"))
                .is_ok(),
            "the reserving launch's own dispatch must pass"
        );
        assert!(
            ownership
                .refuse_if_reserved_elsewhere(Some("launch-b"))
                .is_err()
        );
    }

    /// The slice record outlives the reservation: the reservation guards the
    /// launch, the slice describes its result, and rediscovery needs the
    /// latter long after the former is gone.
    /// The user-facing half of the exclusion above. A refusal has to say which
    /// launch holds the machine and which coordinator is driving it, or the
    /// operator has no way to tell a stuck reservation from a busy one.
    #[test]
    fn a_refused_local_action_names_the_launch_and_its_coordinator() {
        let ownership = SliceOwnership::new();
        assert!(
            ownership.refuse_if_reserved_elsewhere(None).is_ok(),
            "an unreserved daemon refuses nothing"
        );

        ownership.try_reserve("launch-a", "cn-robot-7");

        let refusal = ownership
            .refuse_if_reserved_elsewhere(None)
            .expect_err("a user-typed action must be refused while a launch holds the machine");
        assert!(refusal.contains("launch-a"), "got: {refusal}");
        assert!(refusal.contains("cn-robot-7"), "got: {refusal}");
        assert!(refusal.contains("stack reset"), "got: {refusal}");

        assert!(
            ownership
                .refuse_if_reserved_elsewhere(Some("launch-a"))
                .is_ok(),
            "the holding launch's own dispatch must pass"
        );
        assert!(
            ownership
                .refuse_if_reserved_elsewhere(Some("launch-b"))
                .is_err(),
            "another launch's dispatch is still excluded"
        );
    }

    #[test]
    fn the_slice_record_outlives_the_reservation() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        ownership.record_slice(LaunchIdentity::new("launch-a", "cn-robot-7"));
        ownership.release("launch-a");

        assert_eq!(ownership.held_reservation(), None);
        assert_eq!(
            ownership.slice(),
            Some(LaunchIdentity::new("launch-a", "cn-robot-7")),
            "a released reservation must leave the slice discoverable"
        );
    }

    #[test]
    fn a_daemon_with_no_federated_launch_reports_no_slice() {
        assert_eq!(SliceOwnership::new().slice(), None);
    }

    #[test]
    fn clearing_drops_both_the_reservation_and_the_slice() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        ownership.record_slice(LaunchIdentity::new("launch-a", "cn-robot-7"));

        ownership.clear();

        assert_eq!(ownership.held_reservation(), None);
        assert_eq!(ownership.slice(), None);
    }
}
