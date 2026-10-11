//! The budgets a goal that adds nodes runs under when its caller sets none,
//! and the silence a caller waits for before it stops waiting.

use std::time::Duration;

use core_node_api::encoding::{DEFAULT_IDLE_TIMEOUT_SECS, StackBudgets};

/// The idle budget in seconds of the build phase when the caller sets none:
/// the default of `peppy node build --idle-timeout` and of
/// `--node-build-idle-timeout-secs`.
///
/// It is smaller than [`DEFAULT_IDLE_TIMEOUT_SECS`] on purpose: the build's
/// idle clock does not depend on sporadic subprocess chatter. It also resets
/// whenever bytes land on disk (image download, cache writes, SIF assembly,
/// compiler output) or the build's processes burn CPU (a compiler holding one
/// crate for minutes), sampled every 5 s. So 180 s of silence is 36
/// consecutive samples with no growth and no CPU: not "maybe still working",
/// but wedged. Add and run keep the 600 s default because they have no such
/// signal: add has quiet delta-resolution stretches, and run has no progress
/// proxy at all (nodes legitimately initialize silently).
pub const DEFAULT_BUILD_IDLE_TIMEOUT_SECS: u64 = 180;

/// The budgets of a launch, a build or a join whose caller sets none: 600 s
/// of silence for the add phase, [`DEFAULT_BUILD_IDLE_TIMEOUT_SECS`] for the
/// build phase, 600 s for the run phase, no overall deadline and no
/// environment. These are the only defaults of these budgets: the flags of
/// `peppy stack launch`, `build` and `join` default to them, and the daemon
/// has none of its own, because every goal carries its budgets.
pub const DEFAULT_BUDGETS: StackBudgets = StackBudgets {
    env_vars: Vec::new(),
    node_add_idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
    node_build_idle_timeout_secs: DEFAULT_BUILD_IDLE_TIMEOUT_SECS,
    node_run_idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
    max_timeout_secs: None,
};

/// The time a caller waits past a budget of the daemon before it gives up
/// on the daemon. The daemon fails a phase that goes silent for its budget
/// and sends that failure as the result, so a caller that waits this much
/// longer gets the daemon's own error, which names the phase, and not a
/// silence of its own.
pub const DAEMON_RESPONSE_GRACE: Duration = Duration::from_secs(60);

/// How long a goal that runs under `budgets` can stay silent before its
/// caller stops waiting: the largest phase budget, because one phase runs at
/// a time, plus [`DAEMON_RESPONSE_GRACE`]. 660 s for [`DEFAULT_BUDGETS`].
pub fn silence_window(budgets: &StackBudgets) -> Duration {
    let largest_phase_budget = budgets
        .node_add_idle_timeout_secs
        .max(budgets.node_build_idle_timeout_secs)
        .max(budgets.node_run_idle_timeout_secs);
    Duration::from_secs(largest_phase_budget).saturating_add(DAEMON_RESPONSE_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults a join sends from the CLI and over MCP.
    #[test]
    fn the_default_budgets_give_the_build_less_silence_and_set_no_deadline() {
        assert_eq!(
            DEFAULT_BUDGETS,
            StackBudgets::new(600, 180, 600, None),
            "add 600 s, build 180 s, run 600 s, no overall deadline"
        );
        assert!(DEFAULT_BUDGETS.env_vars.is_empty());
    }

    #[test]
    fn the_silence_window_of_the_default_budgets_is_660_s() {
        assert_eq!(silence_window(&DEFAULT_BUDGETS), Duration::from_secs(660));
    }

    /// One phase runs at a time, so the largest budget sizes the window,
    /// whichever phase holds it.
    #[test]
    fn the_silence_window_follows_the_largest_phase_budget() {
        for budgets in [
            StackBudgets::new(900, 1, 1, None),
            StackBudgets::new(1, 900, 1, None),
            StackBudgets::new(1, 1, 900, Some(5)),
        ] {
            assert_eq!(silence_window(&budgets), Duration::from_secs(960));
        }
    }

    #[test]
    fn the_silence_window_saturates_at_the_largest_duration() {
        let budgets = StackBudgets::new(u64::MAX, 1, 1, None);
        assert_eq!(silence_window(&budgets), Duration::MAX);
    }
}
