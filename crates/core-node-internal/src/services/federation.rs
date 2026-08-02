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
//! The lease covers a coordinator that LEFT; a lost release from one that
//! stayed is covered by takeover. The release at the end of a launch is
//! best-effort, so a coordinator whose messaging failed mid-launch ends its
//! launch still holding this daemon while remaining present on the
//! federation. A coordinator drives one launch at a time, which makes the
//! recovery safe: reserving for a NEW launch id from the coordinator already
//! holding this daemon proves the held launch is over, and
//! [`SliceOwnership::try_reserve`] hands the reservation to the new launch.
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

use core_node_api::LaunchScoped;
use core_node_api::encoding::LaunchIdentity;
use parking_lot::Mutex;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The lifetime of one reservation, handed to the presence watch supervising
/// it.
///
/// A lease ends exactly when its reservation does, whichever way that happens:
/// the coordinator releasing it, `stack reset` clearing it, or the watch
/// dropping it because the coordinator left. Ending it is what stops the watch,
/// so a daemon that serves launch after launch holds one presence subscription
/// per HELD reservation rather than one per launch it has ever taken part in.
///
/// It is also the watch's identity, which is why
/// [`SliceOwnership::release_because_coordinator_gone`] takes a lease rather
/// than a coordinator name: a coordinator reserves this daemon again for every
/// launch it drives, and a watch left over from an earlier reservation of that
/// same coordinator must not be able to drop the current one.
#[derive(Debug, Clone)]
pub struct Lease(CancellationToken);

impl Lease {
    fn new() -> Self {
        Self(CancellationToken::new())
    }

    fn end(&self) {
        self.0.cancel();
    }

    fn has_ended(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Resolves when the reservation this lease covers ends.
    pub async fn ended(&self) {
        self.0.cancelled().await;
    }
}

/// A reservation currently held by this daemon.
#[derive(Debug)]
struct HeldReservation {
    launch_id: String,
    coordinator_core_node: String,
    /// Ends when this reservation does; see [`Lease`].
    lease: Lease,
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
#[derive(Debug, Clone)]
pub enum ReserveOutcome {
    /// The reservation was just taken for the requesting launch, and owes that
    /// reservation the one presence watch supervising it. Carrying the lease
    /// is what pairs the two: a watch can only be spawned for a reservation
    /// this call created, and it can only ever act on that one.
    Reserved { lease: Lease },
    /// The requesting launch already held it. Distinct from [`Self::Reserved`]
    /// because the two owe the caller different work: a coordinator retrying a
    /// dropped reply must not refuse itself, but it must not get a second
    /// coordinator-presence watch either, since the one the first attempt
    /// spawned is still supervising this same reservation.
    AlreadyHeld,
    /// The requesting coordinator already held this daemon, but for a launch
    /// it is no longer driving, so the reservation now belongs to the
    /// requesting launch. A coordinator drives one launch at a time, which is
    /// what makes the handover safe: a new launch id from the holding
    /// coordinator proves the held launch ended and its release never landed.
    /// Like [`Self::AlreadyHeld`], no new presence watch is owed: the launch id
    /// changes inside the reservation that is already held, so its lease, and
    /// the watch holding that lease, carry over.
    TookOverFromSameCoordinator { stale_launch_id: String },
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
    /// Re-reserving for the same launch succeeds (as
    /// [`ReserveOutcome::AlreadyHeld`]): the exchange is a network round trip,
    /// so a coordinator whose reply was lost must be able to retry without
    /// deadlocking against its own reservation.
    ///
    /// Reserving for a NEW launch from the coordinator already holding this
    /// daemon also succeeds (as
    /// [`ReserveOutcome::TookOverFromSameCoordinator`]). The release at the
    /// end of a launch is best-effort, and the presence lease only clears a
    /// reservation whose coordinator LEFT the federation: a coordinator whose
    /// messaging failed mid-launch stays present, so a lost release would
    /// otherwise hold this daemon against every retry until a `stack reset`.
    /// A coordinator drives one launch at a time, so a new launch id from the
    /// holding coordinator proves the held launch is over.
    pub fn try_reserve(&self, launch_id: &str, coordinator: &str) -> ReserveOutcome {
        let mut state = self.state.lock();
        match state.reservation.as_mut() {
            Some(held) if held.launch_id == launch_id => ReserveOutcome::AlreadyHeld,
            Some(held) if held.coordinator_core_node == coordinator => {
                // The launch id changes inside the reservation that is already
                // held, rather than the reservation being replaced, so the
                // lease the watch is holding stays the live one.
                let stale_launch_id = std::mem::replace(&mut held.launch_id, launch_id.to_owned());
                ReserveOutcome::TookOverFromSameCoordinator { stale_launch_id }
            }
            Some(held) => ReserveOutcome::HeldByAnotherLaunch {
                launch_id: held.launch_id.clone(),
                coordinator_core_node: held.coordinator_core_node.clone(),
            },
            None => {
                let lease = Lease::new();
                state.reservation = Some(HeldReservation {
                    launch_id: launch_id.to_owned(),
                    coordinator_core_node: coordinator.to_owned(),
                    lease: lease.clone(),
                });
                ReserveOutcome::Reserved { lease }
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
        match state.reservation.as_ref() {
            Some(held) if held.launch_id != launch_id => false,
            Some(_) => {
                end_held_reservation(&mut state);
                true
            }
            None => true,
        }
    }

    /// Drops the reservation `lease` covers, because the coordinator holding it
    /// vanished from the federation. This is what keeps a dead coordinator from
    /// wedging a machine until its next daemon restart.
    ///
    /// Takes the lease rather than a coordinator name so a watch that outlived
    /// its reservation frees nothing. A lease ends with the reservation it
    /// covers, and only one is ever live, so a watch whose lease has ended
    /// cannot drop the reservation a later launch took, not even one driven by
    /// the same coordinator it was watching.
    ///
    /// Returns the launch that was released, if any.
    pub fn release_because_coordinator_gone(&self, lease: &Lease) -> Option<String> {
        let mut state = self.state.lock();
        if lease.has_ended() {
            return None;
        }
        let launch_id = state.reservation.as_ref()?.launch_id.clone();
        end_held_reservation(&mut state);
        Some(launch_id)
    }

    /// The refusal a node action owes its caller when this machine is
    /// committed to a federated launch other than the one asking.
    ///
    /// Takes the goal itself, bounded on [`LaunchScoped`], rather than a bare
    /// `Option<&str>`: which actions are launch-scoped is declared once in the
    /// core-node registry (`scope: launch`), and that declaration is what emits
    /// the impl. An action that carries no launch scope therefore cannot be
    /// passed here at all, and no call site gets to decide for itself where a
    /// goal's launch id comes from.
    ///
    /// Every node action shares it so the three cannot explain the same
    /// situation three different ways.
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
        goal: &impl LaunchScoped,
    ) -> std::result::Result<(), String> {
        let launch_id = goal.launch_id();
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
        end_held_reservation(&mut state);
        state.slice = None;
    }
}

/// Drops the held reservation and ends its lease, which is what stops the
/// presence watch supervising it. Every way out of a reservation goes through
/// here, so none of them can leave a watch running over a reservation that is
/// no longer held.
fn end_held_reservation(state: &mut OwnershipState) {
    if let Some(held) = state.reservation.take() {
        held.lease.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a real launch-scoped goal. The gate only ever reads
    /// `launch_id()`, so exercising it does not need a whole `NodeRunGoal`.
    struct Goal(Option<&'static str>);

    impl LaunchScoped for Goal {
        fn launch_id(&self) -> Option<&str> {
            self.0
        }
    }

    /// A goal a user typed: no launch of its own, so any held reservation
    /// excludes it.
    const LOCAL: Goal = Goal(None);

    /// Reserves and returns the lease the presence watch would be given.
    ///
    /// Every test that later acts as that watch needs it, and a reservation
    /// that is not fresh has none, so the panic states which outcome a test
    /// expected rather than leaving it as an unwrapped `None`.
    fn reserve_expecting_a_fresh_lease(
        ownership: &SliceOwnership,
        launch_id: &str,
        coordinator: &str,
    ) -> Lease {
        let ReserveOutcome::Reserved { lease } = ownership.try_reserve(launch_id, coordinator)
        else {
            panic!("`{launch_id}` should have taken a fresh reservation on `{coordinator}`");
        };
        lease
    }

    #[test]
    fn a_free_daemon_accepts_a_reservation() {
        let ownership = SliceOwnership::new();
        reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
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

        let ReserveOutcome::HeldByAnotherLaunch {
            launch_id,
            coordinator_core_node,
        } = ownership.try_reserve("launch-b", "cn-robot-9")
        else {
            panic!("a second coordinator must be refused while `launch-a` holds the daemon");
        };
        assert_eq!(
            (launch_id.as_str(), coordinator_core_node.as_str()),
            ("launch-a", "cn-robot-7"),
            "the refusal must name the launch holding the daemon and who drives it"
        );
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-a".to_owned(), "cn-robot-7".to_owned())),
            "a refused reservation must not disturb the held one"
        );
    }

    /// The exchange is a network round trip, so a coordinator whose reply was
    /// lost has to be able to retry without deadlocking against itself. The
    /// retry is reported as `AlreadyHeld` so the handler knows not to spawn a
    /// second presence watch over the one reservation.
    #[test]
    fn re_reserving_the_same_launch_reports_the_reservation_as_already_held() {
        let ownership = SliceOwnership::new();
        reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
        assert!(
            matches!(
                ownership.try_reserve("launch-a", "cn-robot-7"),
                ReserveOutcome::AlreadyHeld
            ),
            "a retry of the same launch owes no second presence watch"
        );
    }

    /// The lost-release wedge: a launch whose release never landed (the
    /// coordinator's messaging failed mid-launch while the coordinator stayed
    /// present, so the lease never fired) must not hold this daemon against
    /// every retry until a `stack reset`. A coordinator drives one launch at
    /// a time, so a new launch id from the holding coordinator proves the
    /// held launch is over, and the reservation moves to it.
    #[test]
    fn the_same_coordinators_next_launch_takes_over_a_stale_reservation() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");

        let ReserveOutcome::TookOverFromSameCoordinator { stale_launch_id } =
            ownership.try_reserve("launch-b", "cn-robot-7")
        else {
            panic!("the holding coordinator's next launch must take over its stale reservation");
        };
        assert_eq!(stale_launch_id, "launch-a");
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-b".to_owned(), "cn-robot-7".to_owned())),
            "the reservation must now belong to the launch the coordinator is driving"
        );
    }

    /// The takeover keeps the lease intact: the reservation the watch is
    /// supervising changes launch, it is not replaced, so the coordinator
    /// vanishing still frees the machine after the handover.
    #[test]
    fn a_taken_over_reservation_still_releases_when_the_coordinator_vanishes() {
        let ownership = SliceOwnership::new();
        let lease = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
        ownership.try_reserve("launch-b", "cn-robot-7");

        assert!(
            !lease.has_ended(),
            "a takeover must not end the lease the watch is holding"
        );
        assert_eq!(
            ownership.release_because_coordinator_gone(&lease),
            Some("launch-b".to_owned())
        );
        assert_eq!(ownership.held_reservation(), None);
    }

    #[test]
    fn releasing_frees_the_daemon_for_the_next_launch() {
        let ownership = SliceOwnership::new();
        ownership.try_reserve("launch-a", "cn-robot-7");
        assert!(ownership.release("launch-a"));
        assert_eq!(ownership.held_reservation(), None);
        reserve_expecting_a_fresh_lease(&ownership, "launch-b", "cn-robot-9");
    }

    /// The watch is a subscription, so a reservation that ends any other way
    /// has to stop it. Otherwise a daemon serving launch after launch keeps one
    /// presence watch per launch it ever took part in, all of them supervising
    /// a reservation that is long gone.
    #[test]
    fn every_way_out_of_a_reservation_ends_its_lease() {
        let ownership = SliceOwnership::new();

        let released = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
        ownership.release("launch-a");
        assert!(released.has_ended(), "a release must stop the watch");

        let reset = reserve_expecting_a_fresh_lease(&ownership, "launch-b", "cn-robot-7");
        ownership.clear();
        assert!(reset.has_ended(), "a `stack reset` must stop the watch");

        let vanished = reserve_expecting_a_fresh_lease(&ownership, "launch-c", "cn-robot-7");
        ownership.release_because_coordinator_gone(&vanished);
        assert!(
            vanished.has_ended(),
            "a watch that released its own reservation has nothing left to watch"
        );
    }

    /// A launch refused because another one holds the daemon leaves the holder
    /// supervised: the refusal changes nothing, so the watch must keep running.
    #[test]
    fn a_refused_reservation_leaves_the_holders_lease_alone() {
        let ownership = SliceOwnership::new();
        let lease = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");

        ownership.try_reserve("launch-b", "cn-robot-9");

        assert!(!lease.has_ended());
        assert_eq!(
            ownership.release_because_coordinator_gone(&lease),
            Some("launch-a".to_owned())
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
        let lease = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");

        assert_eq!(
            ownership.release_because_coordinator_gone(&lease),
            Some("launch-a".to_owned())
        );
        assert_eq!(ownership.held_reservation(), None);
        reserve_expecting_a_fresh_lease(&ownership, "launch-b", "cn-robot-9");
    }

    /// A watch belonging to an already-released reservation must not free a
    /// reservation a different coordinator has since taken.
    #[test]
    fn a_stale_coordinator_watch_does_not_release_someone_elses_reservation() {
        let ownership = SliceOwnership::new();
        let stale = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
        ownership.release("launch-a");
        ownership.try_reserve("launch-b", "cn-robot-9");

        assert_eq!(ownership.release_because_coordinator_gone(&stale), None);
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-b".to_owned(), "cn-robot-9".to_owned()))
        );
    }

    /// The same trap, sprung by the coordinator a stale watch is actually
    /// watching. A coordinator reserves this daemon once per launch it drives,
    /// so between one launch's release and the next launch's reservation there
    /// is a window where a watch left over from the first is still live. It
    /// must not drop the second launch's reservation, whatever it sees in that
    /// window: the coordinator is present, and the launch it is driving is not
    /// the one that watch was supervising.
    #[test]
    fn a_stale_watch_does_not_release_the_same_coordinators_next_reservation() {
        let ownership = SliceOwnership::new();
        let stale = reserve_expecting_a_fresh_lease(&ownership, "launch-a", "cn-robot-7");
        ownership.release("launch-a");
        let live = reserve_expecting_a_fresh_lease(&ownership, "launch-b", "cn-robot-7");

        assert_eq!(ownership.release_because_coordinator_gone(&stale), None);
        assert_eq!(
            ownership.held_reservation(),
            Some(("launch-b".to_owned(), "cn-robot-7".to_owned())),
            "the launch the coordinator is driving must keep the daemon it reserved"
        );
        assert_eq!(
            ownership.release_because_coordinator_gone(&live),
            Some("launch-b".to_owned()),
            "the live watch must still be the one that can free the machine"
        );
    }

    /// The reservation covers the whole machine, not just the launch action:
    /// a coordinator dispatching to a peer drives the NODE actions, which have
    /// their own gates, so local node work has to consult this to stay out of
    /// the way. The refusal also has to say which launch holds the machine,
    /// which coordinator is driving it, and how to clear it, or the operator
    /// has no way to tell a stuck reservation from a busy one.
    #[test]
    fn local_work_is_excluded_while_another_launch_holds_the_daemon() {
        let ownership = SliceOwnership::new();
        assert!(ownership.refuse_if_reserved_elsewhere(&LOCAL).is_ok());

        ownership.try_reserve("launch-a", "cn-robot-7");

        let refusal = ownership
            .refuse_if_reserved_elsewhere(&LOCAL)
            .expect_err("a local action names no launch, so it is excluded");
        assert!(refusal.contains("launch-a"), "got: {refusal}");
        assert!(
            refusal.contains("cn-robot-7"),
            "the refusal must name the coordinator to wait on; got: {refusal}"
        );
        assert!(
            refusal.contains("stack reset"),
            "the refusal must name the escape hatch; got: {refusal}"
        );

        assert!(
            ownership
                .refuse_if_reserved_elsewhere(&Goal(Some("launch-a")))
                .is_ok(),
            "the reserving launch's own dispatch must pass"
        );
        assert!(
            ownership
                .refuse_if_reserved_elsewhere(&Goal(Some("launch-b")))
                .is_err()
        );
    }

    /// The slice record outlives the reservation: the reservation guards the
    /// launch, the slice describes its result, and rediscovery needs the
    /// latter long after the former is gone.
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
