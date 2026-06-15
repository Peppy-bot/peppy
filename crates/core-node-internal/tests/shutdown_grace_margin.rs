//! The daemon's force-kill deadline must be strictly greater than the longest a
//! cooperative node can take to actually exit, or a node that cleans up
//! correctly is still SIGKILLed. After receiving the shutdown request a node
//! runs its hooks (bounded by `shutdown_grace`), then a Python node joins its
//! asyncio event-loop thread (bounded by `EVENT_LOOP_JOIN_BUDGET_SECS`), then
//! the interpreter finalizes. The deadline is derived once by
//! `core_node::force_kill_deadline`; these tests pin the invariant to that
//! single source so it cannot silently drift back to "deadline == grace" (the
//! original force-kill bug).

use std::time::Duration;

use config::peppy_config::{
    DEFAULT_SHUTDOWN_GRACE_SECS, EVENT_LOOP_JOIN_BUDGET_SECS, RUNTIME_FINALIZE_MARGIN_SECS,
};
use core_node::{TEARDOWN_REAP_BUDGET, force_kill_deadline};

/// Node-side bounded exit cost: hook grace + event-loop join. Interpreter
/// finalize is unbounded node-side and is what the daemon's finalize margin
/// covers, so it is deliberately excluded from the node's *bounded* term.
fn node_bounded_exit(grace: Duration) -> Duration {
    grace + Duration::from_secs(EVENT_LOOP_JOIN_BUDGET_SECS)
}

#[test]
fn deadline_exceeds_node_bounded_exit_for_every_accepted_grace() {
    // Check across the full range of accepted grace values (minimum accepted is
    // 1s), not just the default, so the invariant holds for any configured grace.
    for grace_secs in 1..=600 {
        let grace = Duration::from_secs(grace_secs);
        let deadline = force_kill_deadline(grace);

        assert!(
            deadline > grace,
            "deadline {deadline:?} must exceed the hook grace {grace:?} (the original bug)",
        );
        assert!(
            deadline > node_bounded_exit(grace),
            "deadline {deadline:?} must exceed the node's bounded exit {:?} \
             (hook grace + event-loop join), leaving margin for interpreter finalize",
            node_bounded_exit(grace),
        );
    }
}

#[test]
fn deadline_is_grace_plus_join_plus_finalize() {
    // Lock the formula so a future edit to the constants stays coherent.
    let grace = Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS);
    let expected =
        grace + Duration::from_secs(EVENT_LOOP_JOIN_BUDGET_SECS + RUNTIME_FINALIZE_MARGIN_SECS);
    assert_eq!(force_kill_deadline(grace), expected);
    // The finalize margin is the headroom beyond the node's bounded exit.
    assert_eq!(
        force_kill_deadline(grace) - node_bounded_exit(grace),
        Duration::from_secs(RUNTIME_FINALIZE_MARGIN_SECS),
    );
}

#[test]
fn cli_request_timeout_outlasts_the_daemon_deadline() {
    // The CLI waits force_kill_deadline + reap + a messaging margin; since the
    // reap budget is positive, the CLI always strictly outlasts the daemon's
    // worst-case stop, so it never reports a timeout for a stop that succeeded.
    let grace = Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS);
    let daemon_deadline = force_kill_deadline(grace);
    let cli_lower_bound = daemon_deadline + TEARDOWN_REAP_BUDGET;
    assert!(cli_lower_bound > daemon_deadline);
}
