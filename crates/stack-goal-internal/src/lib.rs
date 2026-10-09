//! The one way to send a stack goal to a peppy daemon, follow its progress
//! and read its result, and the default budgets of a goal that adds nodes.
//! `peppy stack launch`, `build`, `join` and `remove` drive their goals
//! through it, and so does the daemon bridge of the built-in MCP server. Each
//! caller keeps what is its own: the CLI its terminal output, its watchdog
//! and its listing of the log files, the bridge its MCP texts and its
//! progress window.
//!
//! # The goal path
//!
//! 1. [`send`] a [`StackGoal`] along a [`DaemonRoute`]. The daemon admits or
//!    refuses the goal at once: [`send`] returns a [`RunningStackGoal`],
//!    which holds the goal's log file, or [`SendError::Rejected`] with the
//!    daemon's reason.
//! 2. Call [`RunningStackGoal::next`] until it gives
//!    [`StackGoalEvent::Ended`]. Each [`StackGoalEvent::Feedback`] is one
//!    line of the goal's progress, in order: its `step`
//!    ([`LaunchFeedbackStep::phase_label`] gives launch, add, build or run),
//!    its `stream` and its `line`. The last event is the goal's
//!    [`LaunchResult`]. A goal that ran and failed ends with `success` false
//!    and its `error_message`. A goal with no result gives a
//!    [`FollowError`]: the daemon is gone, the daemon dropped the goal, or
//!    its result expired or did not arrive.
//! 3. A caller applies its own silence window and its own cancel by
//!    selecting on [`RunningStackGoal::next`], which is cancel safe. The
//!    goal goes on when the caller stops waiting: the client sends no cancel,
//!    and a stack change ignores one. A caller that stops waiting hands the
//!    goal to a task with [`RunningStackGoal::follow_to_end`], which follows
//!    it until the daemon's work ends.
//!
//! The copies of the stack, with the name and the option of each, come from
//! `peppylib::stack::list`, which asks the daemon a node is bound to.
//!
//! # Budgets
//!
//! [`DEFAULT_BUDGETS`] are the budgets of a launch, a build or a join whose
//! caller sets none: 600 s of silence for the add phase, 180 s for the build
//! phase, 600 s for the run phase and no overall deadline. A caller that
//! waits for a silent goal waits [`silence_window`] of its budgets, the
//! largest phase budget plus [`DAEMON_RESPONSE_GRACE`], so the daemon's own
//! error for the silent phase comes first. A removal carries no budget: the
//! daemon stops the instances of the copy one at a time, each within its
//! teardown budget, and on its own machine it sends a line before it stops
//! each one.
//!
//! # A node that adds a copy to its own stack
//!
//! The daemon bridge of the MCP server runs as a node on the coordinator, so
//! it sends its goals to the daemon it is bound to. A join with no
//! selection, no argument and no environment, placed on the coordinator,
//! under the default budgets, relayed as `{step}: {line}`, with a cancel and
//! a silence window of the caller's own:
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use core_node_api::encoding::{LaunchResult, StackJoinGoal};
//! use peppylib::runtime::NodeRunner;
//! use stack_goal::{DEFAULT_BUDGETS, DaemonRoute, SendError, StackGoalEvent};
//!
//! enum Outcome {
//!     Refused(String),
//!     Ended(LaunchResult),
//!     NoResult(String),
//!     StoppedWaiting,
//! }
//!
//! async fn add_copy(
//!     node: &NodeRunner,
//!     name: config::runtime::Name,
//!     option: &str,
//!     window: Duration,
//!     cancelled: impl std::future::Future<Output = ()>,
//!     relay: impl Fn(String),
//! ) -> Outcome {
//!     let goal = StackJoinGoal::new(name, option, DEFAULT_BUDGETS);
//!     let mut running = match stack_goal::send(DaemonRoute::own_daemon(node), &goal).await {
//!         Ok(running) => running,
//!         Err(SendError::Rejected { reason, .. }) => return Outcome::Refused(reason),
//!         Err(error) => return Outcome::NoResult(error.to_string()),
//!     };
//!     relay(format!("launch: the daemon accepted the goal; log file {}", running.log_path().display()));
//!     tokio::pin!(cancelled);
//!     loop {
//!         tokio::select! {
//!             event = running.next() => match event {
//!                 Ok(StackGoalEvent::Feedback(feedback)) => {
//!                     relay(format!("{}: {}", feedback.step.phase_label(), feedback.line));
//!                 }
//!                 Ok(StackGoalEvent::Ended(result)) => return Outcome::Ended(result),
//!                 Err(error) => return Outcome::NoResult(error.to_string()),
//!             },
//!             // The silence window starts again at each event.
//!             () = tokio::time::sleep(window) => break,
//!             () = &mut cancelled => break,
//!         }
//!     }
//!     // The join goes on: follow it to its end in the background.
//!     tokio::spawn(running.follow_to_end());
//!     Outcome::StoppedWaiting
//! }
//! ```
//!
//! A removal is the same with `StackRemoveGoal::new(name)`. The result's
//! `error_message` says why a goal failed, `log_path` names the log of the
//! goal, and `node_add_logs`, `node_build_logs` and `node_run_logs` name the
//! log file of each node phase, with `failed` set on the phase that failed.
//!
//! [`LaunchFeedbackStep::phase_label`]: core_node_api::encoding::LaunchFeedbackStep::phase_label
//! [`LaunchResult`]: core_node_api::encoding::LaunchResult

mod budgets;
mod goal;

pub use budgets::{
    DAEMON_RESPONSE_GRACE, DEFAULT_BUDGETS, DEFAULT_BUILD_IDLE_TIMEOUT_SECS, silence_window,
};
pub use goal::{
    DaemonRoute, FollowError, RunningStackGoal, SendError, StackGoal, StackGoalEvent, send,
};
