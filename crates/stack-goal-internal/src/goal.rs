//! Sending a stack goal, following its feedback, cancelling it, and reading
//! how it ended.

use std::path::{Path, PathBuf};
use std::time::Duration;

use core_node_api::ActionGoal;
use core_node_api::encoding::{
    LaunchFeedback, LaunchGoal, LaunchGoalResponse, LaunchOutput, LaunchResult, NodeLogs,
    StackBuildGoal, StackJoinGoal, StackRemoveGoal,
};
use peppylib::core_node::transport::send_goal;
use peppylib::messaging::{
    ActionGoalHandle, CancelState, Failure, GoalAnswer, GoalOutcome, MessengerHandle,
    decode_cancel_ack,
};
use peppylib::runtime::NodeRunner;
use peppylib::{ActionMessenger, PeppyError};
use tracing::debug;

/// How long the daemon has to admit or reject a goal.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the daemon has to answer the request for the result of a goal
/// whose feedback has ended. The daemon holds that result for 30 s after the
/// goal ends.
const RESULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the daemon has to answer a cancel request.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

mod sealed {
    pub trait Sealed {}
    impl Sealed for core_node_api::encoding::LaunchGoal {}
    impl Sealed for core_node_api::encoding::StackBuildGoal {}
    impl Sealed for core_node_api::encoding::StackJoinGoal {}
    impl Sealed for core_node_api::encoding::StackRemoveGoal {}
}

/// A goal of the daemon's stack functions: [`LaunchGoal`],
/// [`StackBuildGoal`], [`StackJoinGoal`] or [`StackRemoveGoal`]. The daemon
/// admits each of them with a [`LaunchGoalResponse`], reports its progress
/// with [`LaunchFeedback`] and completes it with a [`LaunchResult`]. No other
/// type can implement this trait.
pub trait StackGoal: ActionGoal + sealed::Sealed {
    /// The word every message about the goal calls it by.
    const OPERATION: &'static str;
}

impl StackGoal for LaunchGoal {
    const OPERATION: &'static str = "Launch";
}

impl StackGoal for StackBuildGoal {
    const OPERATION: &'static str = "Build";
}

impl StackGoal for StackJoinGoal {
    const OPERATION: &'static str = "Join";
}

impl StackGoal for StackRemoveGoal {
    const OPERATION: &'static str = "Remove";
}

/// Who sends a stack goal, and the daemon that runs it.
#[derive(Clone, Copy)]
pub struct DaemonRoute<'a> {
    /// The session the goal travels on.
    pub messenger: &'a MessengerHandle,
    /// The core node the caller is bound to: its identity on the wire.
    pub caller_core_node: &'a str,
    /// The instance id the caller sends the goal as.
    pub caller_instance_id: &'a str,
    /// The core node whose daemon runs the goal.
    pub daemon_core_node: &'a str,
}

impl<'a> DaemonRoute<'a> {
    /// The route from a node to the daemon it is bound to, which is the
    /// daemon that started it.
    pub fn own_daemon(node: &'a NodeRunner) -> Self {
        let processor = node.processor();
        Self {
            messenger: node.messenger(),
            caller_core_node: processor.bound_core_node(),
            caller_instance_id: processor.bound_instance_id(),
            daemon_core_node: processor.bound_core_node(),
        }
    }
}

/// Why a stack goal did not start. A messaging error is boxed: it is large,
/// and every result of the goal path carries this type.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The goal did not reach the daemon, or the daemon did not answer it.
    #[error("Failed to send {operation} goal: {source}")]
    Unsent {
        operation: &'static str,
        #[source]
        source: Box<PeppyError>,
    },
    /// The daemon answered with an admission that does not decode.
    #[error("Failed to decode goal response: {source}")]
    UndecodableAdmission {
        operation: &'static str,
        #[source]
        source: core_node_api::Error,
    },
    /// The daemon refused the goal: its failure kind and its message, which
    /// names what holds its stack when another operation does.
    #[error("{operation} goal refused ({}): {}", .failure.kind(), .failure.message())]
    Refused {
        operation: &'static str,
        failure: Failure,
    },
}

/// Why a running stack goal gives no end. A messaging error is boxed, as in
/// [`SendError`].
#[derive(Debug, thiserror::Error)]
pub enum FollowError {
    /// The daemon that runs the goal is gone, so the goal has no end to give.
    #[error("{operation} ended without a result: its daemon is gone")]
    DaemonGone { operation: &'static str },
    /// The daemon dropped the goal before it ended it.
    #[error("{operation} was abandoned by its worker before producing a result")]
    Abandoned { operation: &'static str },
    /// The goal ended, and its end was gone before the request for it
    /// arrived.
    #[error("{operation} result expired before it could be fetched")]
    Expired { operation: &'static str },
    /// The request for the end failed.
    #[error("Failed to get {operation} result: {source}")]
    ResultUnavailable {
        operation: &'static str,
        #[source]
        source: Box<PeppyError>,
    },
    /// The daemon completed the goal with a result that does not decode.
    #[error("Failed to decode {operation} result: {source}")]
    UndecodableResult {
        operation: &'static str,
        #[source]
        source: core_node_api::Error,
    },
    /// The goal already gave its last event, and has no other.
    #[error("{operation} already ended")]
    Ended { operation: &'static str },
}

/// Why a cancel did not reach the daemon.
#[derive(Debug, thiserror::Error)]
pub enum CancelError {
    /// The cancel request did not reach the daemon, or the daemon did not
    /// answer it.
    #[error("Failed to cancel {operation}: {source}")]
    Unsent {
        operation: &'static str,
        #[source]
        source: Box<PeppyError>,
    },
}

/// How a stack goal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackGoalOutcome {
    /// The daemon completed the goal: the change is a success.
    Completed(LaunchResult),
    /// The daemon failed the goal: its failure kind and its message.
    Failed(Failure),
    /// A cancel ended the goal, for this reason: its caller cancelled it,
    /// its sender is gone, or a stack reset ended it. The change stopped at
    /// the step it reached and rolled back as a failure there does.
    Cancelled { reason: String },
}

/// The end of a stack goal: how it ended, and the log file of each node
/// phase it ran, with `failed` set on the phase that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackGoalEnd {
    pub outcome: StackGoalOutcome,
    pub node_logs: NodeLogs,
}

/// One event of a running stack goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackGoalEvent {
    /// One line of the goal's output, in the order the daemon sent it.
    Output(LaunchOutput),
    /// The goal's end, which is its last event.
    Ended(StackGoalEnd),
}

/// Where a [`RunningStackGoal`] is in its goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The goal sends feedback until it ends.
    Feedback,
    /// The goal ended, and its end is to be read.
    Result,
    /// The goal gave its last event.
    Ended,
}

/// A stack goal the daemon admitted, followed one event at a time with
/// [`next`](Self::next).
pub struct RunningStackGoal {
    operation: &'static str,
    messenger: MessengerHandle,
    handle: ActionGoalHandle,
    log_path: PathBuf,
    node_logs: NodeLogs,
    stage: Stage,
}

/// Sends `goal` along `route` and returns once the daemon admits it.
///
/// The daemon admits or refuses a goal at once. It refuses at admission only
/// a goal it cannot start: an invalid payload, a busy daemon, a reservation,
/// a goal of the same kind in progress, or a log file it cannot open
/// ([`SendError::Refused`]). It fails everything else after admission, with
/// [`StackGoalOutcome::Failed`].
pub async fn send<G: StackGoal>(
    route: DaemonRoute<'_>,
    goal: &G,
) -> Result<RunningStackGoal, SendError> {
    let operation = G::OPERATION;
    let handle = send_goal(
        goal,
        route.messenger,
        route.caller_core_node,
        route.caller_instance_id,
        Some(route.daemon_core_node),
        ADMISSION_TIMEOUT,
    )
    .await
    .map_err(|source| SendError::Unsent {
        operation,
        source: Box::new(source),
    })?;
    let response = match &handle.goal_reply().answer {
        GoalAnswer::Accepted(response) => response,
        GoalAnswer::Refused(failure) => {
            return Err(SendError::Refused {
                operation,
                failure: failure.clone(),
            });
        }
    };
    let admission = LaunchGoalResponse::decode(response.as_ref())
        .map_err(|source| SendError::UndecodableAdmission { operation, source })?;
    Ok(RunningStackGoal {
        operation,
        messenger: route.messenger.clone(),
        handle,
        log_path: admission.log_path,
        node_logs: NodeLogs::default(),
        stage: Stage::Feedback,
    })
}

impl RunningStackGoal {
    /// The word every message about the goal calls it by.
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The log file of the goal on the daemon's machine.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The next event of the goal: each line of its output in the order the
    /// daemon sent it, then its end. After the end or an error, the goal has
    /// no other event, and a call returns [`FollowError::Ended`].
    ///
    /// Cancel safe: a caller can drop the future at any await point, for
    /// example in a `tokio::select!` with its own silence window or cancel,
    /// and a later call gives the event that the dropped call did not give.
    /// Every feedback message of the goal waits in a buffer that keeps all of
    /// them until the caller reads it. The goal does not stop when the caller
    /// stops waiting: only [`cancel`](Self::cancel) stops it.
    pub async fn next(&mut self) -> Result<StackGoalEvent, FollowError> {
        loop {
            match self.stage {
                Stage::Feedback => {
                    if let Some(output) = self.next_output().await? {
                        return Ok(StackGoalEvent::Output(output));
                    }
                }
                Stage::Result => return self.fetch_end().await.map(StackGoalEvent::Ended),
                Stage::Ended => {
                    return Err(FollowError::Ended {
                        operation: self.operation,
                    });
                }
            }
        }
    }

    /// Asks the daemon to cancel the goal. The daemon stops the change at
    /// the step it reached, rolls back as a failure there does, and ends the
    /// goal cancelled: the caller follows the goal with [`next`](Self::next)
    /// to that end. A goal that already ended keeps its end.
    pub async fn cancel(&self) -> Result<(), CancelError> {
        let operation = self.operation;
        let unsent = |source| CancelError::Unsent {
            operation,
            source: Box::new(source),
        };
        let ack = ActionMessenger::cancel_goal(&self.messenger, &self.handle, CANCEL_TIMEOUT)
            .await
            .map_err(unsent)?;
        let state: CancelState = decode_cancel_ack(&ack.payload_bytes()).map_err(unsent)?;
        debug!(operation, ?state, "the daemon answered the cancel");
        Ok(())
    }

    /// Follows the goal to its end and returns it, without its output. A
    /// caller that stops waiting hands the goal to a task of its own with
    /// this, so the goal is followed until the daemon's work ends:
    /// `tokio::spawn(goal.follow_to_end())`.
    pub async fn follow_to_end(mut self) -> Result<StackGoalEnd, FollowError> {
        loop {
            if let StackGoalEvent::Ended(end) = self.next().await? {
                return Ok(end);
            }
        }
    }

    /// The next line of output, or `None` once the feedback ends, which
    /// moves the goal to its end. The node logs the daemon sends as the last
    /// feedback are kept for the end. A message that does not decode is
    /// skipped. The stage changes only after an await completes, so a
    /// dropped call leaves it as it was.
    async fn next_output(&mut self) -> Result<Option<LaunchOutput>, FollowError> {
        loop {
            match self.handle.on_next_feedback().await {
                Ok(message) => match LaunchFeedback::decode(message.payload_bytes().as_ref()) {
                    Ok(LaunchFeedback::Output(output)) => return Ok(Some(output)),
                    Ok(LaunchFeedback::NodeLogs(logs)) => self.node_logs = logs,
                    Err(error) => {
                        debug!(
                            operation = self.operation,
                            %error,
                            "skipped a feedback message that does not decode"
                        );
                    }
                },
                Err(PeppyError::ActionFeedbackProducerGone { .. }) => {
                    self.stage = Stage::Ended;
                    return Err(FollowError::DaemonGone {
                        operation: self.operation,
                    });
                }
                // The end of the feedback: the daemon ended the goal.
                Err(_) => {
                    self.stage = Stage::Result;
                    return Ok(None);
                }
            }
        }
    }

    /// The end of the goal, whose feedback has ended. A dropped call leaves
    /// the goal at its end, and the next call asks again.
    async fn fetch_end(&mut self) -> Result<StackGoalEnd, FollowError> {
        let operation = self.operation;
        let reply =
            ActionMessenger::request_result(&self.messenger, &self.handle, RESULT_TIMEOUT).await;
        self.stage = Stage::Ended;
        let reply = reply.map_err(|source| FollowError::ResultUnavailable {
            operation,
            source: Box::new(source),
        })?;
        let outcome = match reply.outcome {
            GoalOutcome::Completed(result) => StackGoalOutcome::Completed(
                LaunchResult::decode(result.as_ref())
                    .map_err(|source| FollowError::UndecodableResult { operation, source })?,
            ),
            GoalOutcome::Failed(failure) => StackGoalOutcome::Failed(failure),
            GoalOutcome::Cancelled { reason, .. } => StackGoalOutcome::Cancelled { reason },
            GoalOutcome::Abandoned if self.handle.is_producer_gone() => {
                return Err(FollowError::DaemonGone { operation });
            }
            GoalOutcome::Abandoned => return Err(FollowError::Abandoned { operation }),
            GoalOutcome::Expired => return Err(FollowError::Expired { operation }),
        };
        Ok(StackGoalEnd {
            outcome,
            node_logs: std::mem::take(&mut self.node_logs),
        })
    }
}
