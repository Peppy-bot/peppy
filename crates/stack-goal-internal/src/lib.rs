//! The one way to send a stack goal to a peppy daemon, follow its progress,
//! cancel it and read how it ended, and the default budgets of a goal that
//! adds nodes.
//! `peppy stack launch`, `build`, `join` and `remove` drive their goals
//! through it, and so does the daemon's provider of `stack_copies:v1`, which
//! sends the joins and the removals of its clients to its own daemon. Each
//! caller keeps what is its own: the CLI its terminal output, its watchdog
//! and its listing of the log files, the provider the feedback and the end
//! that it gives each client.
//!
//! # The goal path
//!
//! 1. [`send`] a [`StackGoal`] along a [`DaemonRoute`]. The daemon admits or
//!    refuses a goal at once: [`send`] returns a [`RunningStackGoal`], which
//!    holds the goal's log file, or [`SendError::Refused`] with the failure
//!    kind and the message of the daemon's refusal. The kind is a
//!    [`DaemonFailureKind`]: `busy` says that other work holds the stack and
//!    that the same goal can run once that work ends. A refusal with a kind
//!    that the daemon does not give is [`SendError::UnknownRefusalKind`].
//! 2. Call [`RunningStackGoal::next`] until it gives
//!    [`StackGoalEvent::Ended`]. Each [`StackGoalEvent::Output`] is one line
//!    of the goal's progress, in order: its `step`
//!    ([`LaunchFeedbackStep::phase_label`] gives launch, add, build or run),
//!    its `stream` and its `line`. The last event is the goal's
//!    [`StackGoalEnd`]: its [`StackGoalOutcome`], completed with the goal's
//!    [`LaunchResult`], failed with a failure kind of the daemon and a
//!    message, or cancelled with the reason of the cancel, and the log file
//!    of each node phase the goal ran. A goal with no end gives a
//!    [`FollowError`]: the daemon is gone, the daemon dropped the goal, its
//!    end expired or did not arrive, its failure has a kind that the daemon
//!    does not give, or the caller fell behind its output, which cancels it
//!    ([`FollowError::FellBehind`]).
//! 3. A caller applies its own silence window by selecting on
//!    [`RunningStackGoal::next`], which is cancel safe. The goal goes on when
//!    the caller stops waiting: a caller that stops waiting hands the goal to
//!    a task with [`RunningStackGoal::follow_to_end`], which follows it until
//!    the daemon's work ends.
//! 4. A caller whose own caller cancels calls [`RunningStackGoal::cancel`]:
//!    the daemon stops the change at the step it reached, rolls back as a
//!    failure at that step does, and ends the goal cancelled. The caller
//!    follows the goal to that end. A deadline of the caller sends no
//!    cancel.
//!
//! The daemon ends the goals of a sender that is gone: a process that sent a
//! goal and stops cancels it, with the reason "its sender is gone".
//!
//! # Budgets
//!
//! [`DEFAULT_BUDGETS`] are the budgets of a launch, a build or a join whose
//! caller sets none: 600 s of silence for the add phase, 180 s for the build
//! phase, 600 s for the run phase and no overall deadline. A caller that
//! waits for a silent goal waits [`silence_window`] of its budgets, the
//! largest phase budget plus [`DAEMON_RESPONSE_GRACE`], so the daemon's own
//! error for the silent phase comes first. A removal carries no budget: the
//! daemon stops the instances of the copy one at a time across its
//! machines, each within the teardown budget of its machine, and sends a
//! line before it stops each one.
//!
//! # A node that adds a copy to its own stack
//!
//! A node sends its goals to the daemon it is bound to along
//! [`DaemonRoute::own_daemon`]. A join of a copy on the coordinator with no
//! word, no argument and no environment, under the default budgets, relayed
//! as `{step}: {line}`, with a cancel and a silence window of the caller's
//! own:
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use core_node_api::encoding::{CopyReference, StackJoinGoal};
//! use peppylib::runtime::NodeRunner;
//! use stack_goal::{DEFAULT_BUDGETS, DaemonRoute, SendError, StackGoalEnd, StackGoalEvent};
//!
//! enum Outcome {
//!     Refused(String),
//!     Ended(StackGoalEnd),
//!     NoEnd(String),
//!     StoppedWaiting,
//! }
//!
//! async fn add_copy(
//!     node: &NodeRunner,
//!     copy: CopyReference,
//!     window: Duration,
//!     cancelled: impl std::future::Future<Output = ()>,
//!     relay: impl Fn(String),
//! ) -> Outcome {
//!     let goal = StackJoinGoal::new(copy, DEFAULT_BUDGETS);
//!     let mut running = match stack_goal::send(DaemonRoute::own_daemon(node), &goal).await {
//!         Ok(running) => running,
//!         Err(refusal @ SendError::Refused { .. }) => return Outcome::Refused(refusal.to_string()),
//!         Err(error) => return Outcome::NoEnd(error.to_string()),
//!     };
//!     relay(format!("launch: the daemon accepted the goal; log file {}", running.log_path().display()));
//!     tokio::pin!(cancelled);
//!     loop {
//!         tokio::select! {
//!             event = running.next() => match event {
//!                 Ok(StackGoalEvent::Output(output)) => {
//!                     relay(format!("{}: {}", output.step.phase_label(), output.line));
//!                 }
//!                 Ok(StackGoalEvent::Ended(end)) => return Outcome::Ended(end),
//!                 Err(error) => return Outcome::NoEnd(error.to_string()),
//!             },
//!             // The silence window starts again at each event. The join goes
//!             // on: follow it to its end in the background.
//!             () = tokio::time::sleep(window) => break,
//!             // The caller cancelled: the daemon rolls the join back and ends
//!             // it cancelled.
//!             () = &mut cancelled => {
//!                 if let Err(error) = running.cancel().await {
//!                     relay(error.to_string());
//!                 }
//!                 return match running.follow_to_end().await {
//!                     Ok(end) => Outcome::Ended(end),
//!                     Err(error) => Outcome::NoEnd(error.to_string()),
//!                 };
//!             }
//!         }
//!     }
//!     tokio::spawn(running.follow_to_end());
//!     Outcome::StoppedWaiting
//! }
//! ```
//!
//! A removal is the same with `StackRemoveGoal::new(name)`. A failed goal's
//! failure says why it failed, `log_path` names the log of the goal, and the
//! `node_logs` of its end name the log file of each node phase, with
//! `failed` set on the phase that failed.
//!
//! [`DaemonFailureKind`]: core_node_api::DaemonFailureKind
//! [`LaunchFeedbackStep::phase_label`]: core_node_api::encoding::LaunchFeedbackStep::phase_label
//! [`LaunchResult`]: core_node_api::encoding::LaunchResult

mod budgets;
mod goal;

pub use budgets::{
    DAEMON_RESPONSE_GRACE, DEFAULT_BUDGETS, DEFAULT_BUILD_IDLE_TIMEOUT_SECS, silence_window,
};
pub use goal::{
    CancelError, DaemonRoute, FollowError, RunningStackGoal, SendError, StackGoal, StackGoalEnd,
    StackGoalEvent, StackGoalOutcome, send,
};
