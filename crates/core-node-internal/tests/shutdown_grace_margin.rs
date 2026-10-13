//! The daemon's force-kill deadline must be strictly greater than the longest a
//! cooperative node can take to actually exit, or a node that cleans up
//! correctly is still SIGKILLed. After receiving the shutdown request a node
//! runs its hooks (bounded by `shutdown_grace`), then a Python node joins its
//! asyncio event-loop thread (bounded by `EVENT_LOOP_JOIN_BUDGET_SECS`), then
//! the interpreter finalizes. The deadline is derived once by
//! `core_node::force_kill_deadline`; these tests pin the invariant to that
//! single source, so a deadline equal to the grace fails them.

use std::time::Duration;

use config::peppy_config::{
    DEFAULT_SHUTDOWN_GRACE_SECS, EVENT_LOOP_JOIN_BUDGET_SECS, RUNTIME_FINALIZE_MARGIN_SECS,
};
use core_node::force_kill_deadline;

/// Node-side bounded exit cost: hook grace + event-loop join. Interpreter
/// finalize is unbounded node-side and is what the daemon's finalize margin
/// covers, so it is deliberately excluded from the node's *bounded* term.
fn node_bounded_exit(grace: Duration) -> Duration {
    grace + Duration::from_secs(EVENT_LOOP_JOIN_BUDGET_SECS)
}

#[test]
fn deadline_exceeds_node_bounded_exit_for_every_accepted_grace() {
    // Check every grace from the minimum the config accepts (1 s) up to 600 s,
    // not just the default. The config sets no upper bound; the deadline is
    // linear in the grace, so this range covers the invariant.
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

// The top link of the timeout chain (the CLI request timeout strictly outlasts
// `node_stop_bound`, which holds this deadline) is pinned in the `peppy` crate's
// `commands::node::stop` tests, where the CLI-only `DAEMON_MESSAGING_MARGIN` is
// in scope. It cannot live here: `core-node-internal` is below `peppy` in the
// dependency graph. Together the two give the full CLI > daemon-deadline >
// node-bounded-exit chain.
