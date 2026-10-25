//! Shared accept loop for the built-in core-node actions, and how each of
//! their goals ends.
//!
//! Each built-in action (`node_add`, `node_run`, `node_build`, the stack
//! actions, `stack_benchmark`, `repo_refresh`) is single-goal: it admits one
//! in-flight goal at a time, guarded by a
//! [`super::node::gate::ConcurrencyGate`], and refuses concurrent goals. The
//! loop pulls goals from a [`ConcurrentAction`] and hands each to the
//! action's [`GoalHandler`], which owns the full accept/refuse decision.
//!
//! A goal is refused in its goal envelope, with a failure kind and a
//! message ([`refuse_goal`]). An accepted goal ends with a [`GoalEnding`]:
//! completed with its result, failed with a failure kind and a message, or
//! cancelled with the reason of the cancel. A goal ends cancelled only when
//! a cancel stopped its work ([`WorkStop::Cancelled`]): a work that failed
//! for a cause of its own ends failed, also when a cancel arrives during
//! its rollback. The engine's goal/result rendezvous delivers the ending:
//! the client drains feedback to end-of-stream, then fetches the buffered
//! ending once.
//!
//! Each action of the daemon keeps the goals of a sender that is gone for
//! [`DAEMON_SENDER_GRACE`]. Once the grace passes and the sender did not
//! come back, the engine cancels them with the cause
//! [`peppylib::messaging::CancelCause::SenderGone`], and the work ends them
//! as it ends a cancel of their caller.

use crate::services::raw_calls::RawCallGate;
use config::failure_kind::PeppyFailureKind;
use core_node_api::{ActionId, names};
use peppylib::messaging::{
    CancelCause, ConcurrentAction, Failure, GoalContext, PendingGoal, SenderTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The reason of a goal that a stack reset ended.
pub(crate) const STACK_RESET_REASON: &str = "cancelled by a stack reset";

/// How long each action of the daemon keeps the goals of a sender that is
/// gone. The presence token of a sender that still runs disappears while
/// its router is down: the router watchdog of the daemon restarts a wedged
/// router, and the session of the sender reconnects to the new one. The
/// grace outlasts the longest outage that one restart of the watchdog
/// heals (the daemon asserts it), so a launch, a build or the goal of a
/// participant daemon goes on through a router restart or a short loss of
/// the link between two machines. A command line process that is gone gets
/// its goal cancelled once the grace passes.
pub const DAEMON_SENDER_GRACE: Duration = Duration::from_secs(60);

/// Exposes the built-in action `id` of the daemon on `messenger`, with
/// feedback and with the sender grace of the daemon
/// ([`DAEMON_SENDER_GRACE`]).
pub(crate) async fn expose_daemon_action(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    id: ActionId,
) -> crate::Result<ConcurrentAction> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        id.name(),
        true,
    )
    .await?;
    action.set_sender_grace(DAEMON_SENDER_GRACE);
    Ok(action)
}

/// Handles a single received goal for a built-in single-goal action.
///
/// The handler decodes the goal from [`PendingGoal::request_bytes`], runs
/// admission through the action's gate, and then either refuses it
/// ([`refuse_goal`]) or does its pre-accept setup (create the log file,
/// build the accepted `GoalResponse`), [`PendingGoal::accept`]s it (which
/// registers the goal and replies in one step), and spawns the work task
/// that drives the goal to its [`GoalEnding`].
pub(crate) trait GoalHandler: Clone + Send + 'static {
    fn handle_goal(&self, pending: PendingGoal) -> impl Future<Output = ()> + Send;
}

/// Accept loop shared by all built-in core-node actions. Runs until the goal
/// service closes (the messenger session is torn down). It refuses each goal
/// of the action `id` from an instance of the stack ([`RawCallGate`]) and
/// hands every other goal to `handler`.
pub(crate) async fn run_action_loop<H: GoalHandler>(
    mut action: ConcurrentAction,
    id: ActionId,
    raw_calls: RawCallGate,
    handler: H,
) -> crate::Result<()> {
    while let Some(pending) = action.recv_next_goal().await? {
        if let Some(refusal) = raw_calls.refusal(id.name(), pending.instance_id()) {
            refuse_goal(pending, refusal).await;
            continue;
        }
        handler.handle_goal(pending).await;
    }
    Ok(())
}

/// Accept a pending goal with an encoded `GoalResponse`, returning the per-goal
/// context. Registering the goal and replying both happen inside
/// [`PendingGoal::accept`]. Returns `None` (dropping `pending`, which closes the
/// reply stream so the client's `fire_goal` errors) when the response could not
/// be encoded or the accept reply failed; the caller should then release its
/// admission gate.
pub(crate) async fn accept_goal(
    pending: PendingGoal,
    encoded: PeppyResult<Payload>,
) -> Option<GoalContext> {
    match encoded {
        Ok(payload) => match pending.accept(payload).await {
            Ok(ctx) => Some(ctx),
            Err(err) => {
                debug!("failed to reply accepted goal: {err}");
                None
            }
        },
        Err(err) => {
            debug!("failed to encode goal acceptance: {err}");
            None
        }
    }
}

/// Runs `work` under its goal's cancellation: while the work runs, a cancel
/// request from the goal's caller, or the engine's cancel of a goal whose
/// sender is gone, cancels `token`, and the work finishes through its own
/// cancel path.
///
/// The engine cancels `token` at the cancel signal, before it answers the
/// cancel request: once a cancel is answered, every later step of the work
/// sees `token` cancelled, the steps of a poll of the work in progress at
/// the answer included.
pub(crate) async fn under_goal_cancel<T>(
    goal_ctx: &GoalContext,
    token: &CancellationToken,
    work: impl Future<Output = T>,
) -> T {
    let _link = goal_ctx.cancel_at_signal(token.clone());
    work.await
}

/// Refuses a pending goal with `failure`, in its goal envelope.
pub(crate) async fn refuse_goal(pending: PendingGoal, failure: Failure) {
    if let Err(err) = pending.refuse(failure).await {
        debug!("failed to reply refused goal: {err}");
    }
}

/// The failure of a goal whose payload does not decode.
pub(crate) fn invalid_payload(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        PeppyFailureKind::Rejected,
        format!("invalid payload: {error}"),
    )
}

/// Why the work of an accepted goal gave no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkStop {
    /// A step of the work failed for a cause of its own: its failure kind
    /// and its message.
    Failed(Failure),
    /// A cancel stopped the work at a step, which rolled back as a failure
    /// there does. The failure says where the work stopped, for the log
    /// and the feedback of the goal.
    Cancelled(Failure),
}

impl WorkStop {
    /// The failure of the step the work stopped at.
    pub(crate) fn failure(&self) -> &Failure {
        match self {
            Self::Failed(failure) | Self::Cancelled(failure) => failure,
        }
    }

    /// This stop with `message` as the message of its failure: a stop of
    /// the same cause and of the same kind, which a caller rephrases to say
    /// where it happened.
    pub(crate) fn rephrased(&self, message: impl Into<String>) -> Self {
        let failure = crate::services::response::rephrased(self.failure(), message);
        match self {
            Self::Failed(_) => Self::Failed(failure),
            Self::Cancelled(_) => Self::Cancelled(failure),
        }
    }

    /// The stop that a work reports for `stops`, the stops of its parts in
    /// the order of the parts: the first failure of its own, or, when a
    /// cancel stopped every part that stopped, the first of them. A work
    /// whose part failed for a cause of its own failed, whatever stopped
    /// its other parts.
    pub(crate) fn of_parts(stops: impl IntoIterator<Item = WorkStop>) -> Option<WorkStop> {
        let mut first_cancelled = None;
        for stop in stops {
            match stop {
                Self::Failed(_) => return Some(stop),
                Self::Cancelled(_) => {
                    first_cancelled.get_or_insert(stop);
                }
            }
        }
        first_cancelled
    }
}

impl From<Failure> for WorkStop {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

/// How an accepted goal ends.
#[derive(Debug)]
pub(crate) enum GoalEnding {
    /// The work gave its result: the goal is a success.
    Completed(Payload),
    /// The work failed, with a failure kind and a message.
    Failed(Failure),
    /// A cancel ended the work, for this reason.
    Cancelled(String),
}

impl GoalEnding {
    /// The ending of a goal whose work gave `outcome`, its result encoded:
    /// a work that a cancel stopped ends the goal cancelled, with the reason
    /// of the cancel (`cancel_reason`); a work that failed for a cause of
    /// its own ends it failed, whether a cancel arrived or not; a work that
    /// completed after a cancel completes. A work that a cancel stopped,
    /// with no reason known for the cancel, ends the goal failed with the
    /// failure of the step it stopped at.
    pub(crate) fn of(
        outcome: Result<core_node_api::Result<Payload>, WorkStop>,
        cancel_reason: Option<String>,
    ) -> Self {
        match (outcome, cancel_reason) {
            (Ok(Ok(result)), _) => Self::Completed(result),
            (Ok(Err(error)), _) => Self::Failed(Failure::new(
                PeppyFailureKind::AttemptFailed,
                format!("failed to encode the result: {error}"),
            )),
            (Err(WorkStop::Cancelled(_)), Some(reason)) => Self::Cancelled(reason),
            (Err(WorkStop::Cancelled(failure) | WorkStop::Failed(failure)), _) => {
                Self::Failed(failure)
            }
        }
    }

    /// Ends the goal of `goal_ctx` with this ending. A cancelled goal gives
    /// its reason and no result.
    pub(crate) async fn end(self, goal_ctx: &GoalContext) {
        let ended = match self {
            Self::Completed(result) => goal_ctx.complete(result).await,
            Self::Failed(failure) => goal_ctx.fail(failure).await,
            Self::Cancelled(reason) => goal_ctx.complete_cancelled(reason, Payload::new()).await,
        };
        if let Err(err) = ended {
            debug!("failed to end the goal: {err}");
        }
    }
}

/// Why the goal of `goal_ctx` was cancelled, by one rule for every goal of
/// the daemon: the goal's own cause first (its caller cancelled it, or its
/// sender is gone, [`CancelCause`]), then a stack reset (`reset`). `None`
/// when neither holds: a node add or a node build that a forced one
/// supersedes then gives the reason of the supersede.
pub(crate) fn cancel_reason(goal_ctx: &GoalContext, reset: &CancellationToken) -> Option<String> {
    if let Some(cause) = goal_ctx.cancel_cause() {
        return Some(cause.reason().to_owned());
    }
    reset.is_cancelled().then(|| STACK_RESET_REASON.to_owned())
}

/// Why the goal of `goal_ctx`, whose work only its own cancel stops, was
/// cancelled: the rule of [`cancel_reason`] with no stack reset. A goal with
/// no cause recorded reads as cancelled by its caller.
pub(crate) fn own_cancel_reason(goal_ctx: &GoalContext) -> String {
    goal_ctx
        .cancel_cause()
        .unwrap_or(CancelCause::Caller)
        .reason()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REASON: &str = "cancelled by its caller";

    fn failure(kind: PeppyFailureKind, message: &str) -> Failure {
        Failure::new(kind, message)
    }

    fn ending_of(stop: WorkStop, cancel_reason: Option<&str>) -> GoalEnding {
        GoalEnding::of(Err(stop), cancel_reason.map(str::to_owned))
    }

    /// A work that failed for a cause of its own ends its goal failed with
    /// its kind, also when a cancel arrived during its rollback.
    #[test]
    fn a_work_that_failed_on_its_own_ends_failed_after_a_cancel() {
        let built = failure(
            PeppyFailureKind::AttemptFailed,
            "the build of `arm:v1` failed",
        );

        let ending = ending_of(WorkStop::Failed(built.clone()), Some(REASON));

        let GoalEnding::Failed(ended) = ending else {
            panic!("the goal ends failed: {ending:?}");
        };
        assert_eq!(ended, built);
    }

    /// A work that a cancel stopped ends its goal cancelled, with the reason
    /// of the cancel.
    #[test]
    fn a_work_that_a_cancel_stopped_ends_cancelled() {
        let stopped = failure(
            PeppyFailureKind::AttemptFailed,
            "the stack operation was cancelled",
        );

        let ending = ending_of(WorkStop::Cancelled(stopped.clone()), Some(REASON));
        let GoalEnding::Cancelled(reason) = ending else {
            panic!("the goal ends cancelled: {ending:?}");
        };
        assert_eq!(reason, REASON);

        let ending = ending_of(WorkStop::Cancelled(stopped.clone()), None);
        let GoalEnding::Failed(ended) = ending else {
            panic!("with no reason, the goal ends failed: {ending:?}");
        };
        assert_eq!(ended, stopped);
    }

    /// A work whose part failed for a cause of its own failed, whatever
    /// stopped its other parts; a work whose parts a cancel stopped reports
    /// the first of them.
    #[test]
    fn a_failure_of_a_part_wins_over_a_cancel_of_another_part() {
        let cancelled =
            |message: &str| WorkStop::Cancelled(failure(PeppyFailureKind::AttemptFailed, message));
        let failed = WorkStop::Failed(failure(PeppyFailureKind::Timeout, "no output for 180 s"));

        assert_eq!(
            WorkStop::of_parts([cancelled("first"), failed.clone(), cancelled("third")]),
            Some(failed)
        );
        assert_eq!(
            WorkStop::of_parts([cancelled("first"), cancelled("second")]),
            Some(cancelled("first"))
        );
        assert_eq!(WorkStop::of_parts([]), None);
    }

    const CORE_NODE: &str = "cn-cancel-reason";
    const ACTION: &str = "node_build";

    /// A goal of a daemon on a mock messenger, accepted: the daemon's
    /// context of it, the caller's handle and the messenger they share. The
    /// engine is returned too, as it serves the cancel requests.
    async fn accepted_goal() -> (
        GoalContext,
        peppylib::messaging::ActionGoalHandle,
        peppylib::MessengerHandle,
        ConcurrentAction,
    ) {
        use peppylib::messaging::{FeedbackBound, SenderTarget};
        let messenger = crate::services::tests::started_mock_messenger().await;
        let target = || {
            SenderTarget::node(CORE_NODE, core_node_api::names::CORE_NODE_TAG)
                .expect("a core node identity")
        };
        let mut engine =
            ConcurrentAction::expose(&messenger, CORE_NODE, "core_root", target(), ACTION, true)
                .await
                .expect("the daemon exposes its action");
        let (handle, context) = tokio::join!(
            peppylib::ActionMessenger::send_goal(
                &messenger,
                CORE_NODE,
                "peppy-cli-test",
                target(),
                ACTION,
                None,
                Payload::new(),
                FeedbackBound::new(std::num::NonZeroUsize::MIN),
                std::time::Duration::from_secs(30),
            ),
            async {
                engine
                    .recv_next_goal()
                    .await
                    .expect("the goal service runs")
                    .expect("the goal arrives")
                    .accept(Payload::new())
                    .await
                    .expect("the daemon accepts the goal")
            }
        );
        (
            context,
            handle.expect("the caller sends the goal"),
            messenger,
            engine,
        )
    }

    /// Each action of the daemon keeps the goals of a sender that is gone
    /// for the sender grace of the daemon.
    #[tokio::test]
    async fn a_daemon_action_has_the_sender_grace_of_the_daemon() {
        let messenger = crate::services::tests::started_mock_messenger().await;

        let action = expose_daemon_action(
            &messenger,
            CORE_NODE,
            "core_root",
            CORE_NODE,
            ActionId::NodeBuild,
        )
        .await
        .expect("the daemon exposes its action");

        assert_eq!(action.sender_grace().get(), DAEMON_SENDER_GRACE);
    }

    /// Every goal of the daemon reads why it was cancelled by one rule: the
    /// goal's own cause first, then a stack reset. A goal that its caller
    /// cancelled before a reset gives the reason of its caller.
    #[tokio::test]
    async fn the_cause_of_the_goal_comes_before_a_stack_reset() {
        let (context, handle, messenger, _engine) = accepted_goal().await;
        let reset = CancellationToken::new();
        assert_eq!(
            cancel_reason(&context, &reset),
            None,
            "nothing cancelled it"
        );

        reset.cancel();
        assert_eq!(
            cancel_reason(&context, &reset).as_deref(),
            Some(STACK_RESET_REASON),
            "a reset alone gives its reason"
        );

        peppylib::ActionMessenger::cancel_goal(
            &messenger,
            &handle,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("the daemon answers the cancel");
        assert_eq!(
            cancel_reason(&context, &reset).as_deref(),
            Some(CancelCause::Caller.reason()),
            "the cause of the goal comes before the reset"
        );
        assert_eq!(own_cancel_reason(&context), CancelCause::Caller.reason());
    }

    /// A cancel that the engine answers while a poll of the work is in
    /// progress reaches the token of the work before that poll reads it
    /// again: the step after the answer sees the cancel, with no yield of
    /// the work in between.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_answered_during_a_poll_of_the_work_reaches_its_next_step() {
        let (context, handle, messenger, _engine) = accepted_goal().await;
        let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
        let (answered_tx, answered_rx) = std::sync::mpsc::channel();
        let work = tokio::spawn(async move {
            let token = CancellationToken::new();
            let read_by_the_work = token.clone();
            under_goal_cancel(&context, &token, async move {
                polled_tx.send(()).expect("the test waits for the poll");
                // This poll of the work goes on only once the cancel is
                // answered.
                tokio::task::block_in_place(|| {
                    answered_rx.recv().expect("the test tells the answer")
                });
                read_by_the_work.is_cancelled()
            })
            .await
        });

        polled_rx.await.expect("the work is polled");
        peppylib::ActionMessenger::cancel_goal(
            &messenger,
            &handle,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("the daemon answers the cancel");
        answered_tx.send(()).expect("the work waits for the answer");

        assert!(
            work.await.expect("the work runs to its end"),
            "the step after the answer sees the cancel"
        );
    }

    /// A rephrased stop keeps its cause and its kind.
    #[test]
    fn a_rephrased_stop_keeps_its_cause_and_its_kind() {
        let stopped = WorkStop::Cancelled(failure(PeppyFailureKind::AttemptFailed, "cancelled"));

        let rephrased = stopped.rephrased("failed to build node arm:v1: cancelled");

        assert_eq!(
            rephrased,
            WorkStop::Cancelled(failure(
                PeppyFailureKind::AttemptFailed,
                "failed to build node arm:v1: cancelled"
            ))
        );
    }
}
