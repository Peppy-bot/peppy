//! Action-backed tools: the handler contract an action bridge implements
//! and the context the runtime hands it while a goal runs.
//!
//! The runtime owns the whole MCP side of an action (the task or the call
//! it runs in, confirmation, feedback delivery, the client's cancel, a
//! whole-goal deadline, terminal mapping); the bridge owns the
//! whole Peppy side (firing the goal, draining feedback, forwarding the
//! cancel, awaiting the result, and the progress window of a
//! progress-bound goal, which only the bridge sees the signs of).
//! [`ActionContext`] is the seam between the two, and it hides which of the
//! two MCP surfaces the goal runs on.
//!
//! A client cancels a task with `tasks/cancel`, and a task outlives the
//! connection that started it. A client cancels a call by closing it, which
//! a client that disconnects does too: a goal that runs inside a call is
//! cancelled when its call closes. Stopping the set ends every task (see
//! [`ExposureSet::serve`](crate::ExposureSet::serve)).

use crate::failure::{CallFailure, FailureOutcome, ProviderFailure};
use crate::server::ToolCall;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::task_manager::TaskContext;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How an action bridge finished without a completed result. The runtime
/// maps it onto the MCP task's terminal state, or onto the tool error of
/// the call the goal ran in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExit {
    /// The provider refused the goal, with a failure kind and a message:
    /// the goal never ran. A call answers with the tool error of the
    /// failure; a task ends `failed`, with the failure in its error.
    Refused(ProviderFailure),
    /// The goal ran and failed, with a failure kind and a message. It ends
    /// as a refused goal does.
    Failed(ProviderFailure),
    /// The stack refused the move: the daemon of the fleet refused the goal
    /// on a member that moves a robot, with a failure kind and a message,
    /// and the goal was not sent. It ends as a refused goal does.
    MoveRefused(ProviderFailure),
    /// The Peppy action ended cancelled, whoever cancelled it: the client,
    /// or the provider on its own. A call answers with a tool error that
    /// carries the provider's result; a task ends `cancelled`, and its
    /// status message carries the reason and that result.
    Cancelled(CancelledGoal),
    /// The goal ended in no completion, refusal, failure or cancel that
    /// the provider gave: the goal could not be sent, its feedback
    /// overflowed, its provider abandoned it or gave a failure kind that
    /// the member does not declare, its result expired, or a progress
    /// window went by without a sign of progress. The message says which.
    /// A call answers with a tool error of this text; a task ends `failed`
    /// with it.
    Error(String),
}

impl ActionExit {
    /// The failure the provider gave, with how the call ended, for a goal
    /// that the provider refused or failed.
    pub(crate) fn call_failure(&self) -> Option<CallFailure<'_>> {
        let (outcome, failure) = match self {
            Self::Refused(failure) | Self::MoveRefused(failure) => {
                (FailureOutcome::Refused, failure)
            }
            Self::Failed(failure) => (FailureOutcome::Failed, failure),
            Self::Cancelled(_) | Self::Error(_) => return None,
        };
        Some(CallFailure {
            outcome,
            failure,
            summary: self.to_string(),
        })
    }
}

impl std::fmt::Display for ActionExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(failure) => write!(f, "the provider refused the goal: {failure}"),
            Self::MoveRefused(failure) => write!(f, "the stack refused the move: {failure}"),
            Self::Failed(failure) => write!(f, "the provider failed the goal: {failure}"),
            Self::Cancelled(goal) => write!(f, "{goal}"),
            Self::Error(detail) => write!(f, "the action failed: {detail}"),
        }
    }
}

/// A goal that ended cancelled, with the result its provider ended it
/// with and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledGoal {
    /// The canonical JSON of the result the provider ended the goal with.
    pub result: Value,
    /// Why the goal was cancelled: the reason the provider ended it with.
    pub reason: String,
}

impl CancelledGoal {
    /// The tool result of the goal, for a call that runs it: a tool error
    /// that says the goal was cancelled and why, and carries the provider's
    /// result as structured content and as its JSON text. It is an error
    /// whatever the result says: the goal did not run to its end.
    pub(crate) fn into_tool_result(self) -> CallToolResult {
        let summary = self.to_string();
        let mut result = CallToolResult::structured_error(self.result);
        result.content.insert(0, ContentBlock::text(summary));
        result
    }

    /// The status message of a task whose goal ended cancelled: what
    /// [`Self::into_tool_result`] says, the provider's result as JSON
    /// after it. A `cancelled` task carries no result, so this is where its
    /// client reads why the goal was cancelled.
    pub(crate) fn status_message(&self) -> String {
        format!("{self}: {}", self.result)
    }
}

impl std::fmt::Display for CancelledGoal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the action was cancelled: {}", self.reason)
    }
}

/// The MCP surface a goal runs on, chosen per call by the client's declared
/// capabilities.
#[derive(Clone)]
pub(crate) enum ActionSurface {
    /// An MCP task, for a client that declared the tasks extension:
    /// feedback is the task's status message and `tasks/cancel` is the
    /// cancel signal.
    Task(TaskContext),
    /// The `tools/call` that started the goal, for a client without the
    /// extension: feedback is relayed as progress notifications on that
    /// call, and `closed`, which the transport cancels when the call
    /// closes, is the cancel signal. In Streamable HTTP a client cancels a
    /// call by closing it, and a client that disconnects closes it too: the
    /// endpoint cannot tell the two apart, and takes both as a cancel.
    ///
    /// `feedback` is the relay to the client, which holds one message: a
    /// report waits while the client has not taken the message before it.
    /// The relay stops when the call closes, and a report after that drops
    /// its message at once.
    Call {
        feedback: mpsc::Sender<String>,
        closed: CancellationToken,
    },
}

/// The runtime-side surface an action bridge drives while its goal runs.
#[derive(Clone)]
pub struct ActionContext {
    pub(crate) surface: ActionSurface,
}

impl ActionContext {
    /// Publishes a feedback message to the client: as the task's status
    /// message, which `tasks/get` reports, or as a progress notification on
    /// the call the goal runs in.
    ///
    /// On a task it returns at once. On a call it returns once the relay to
    /// the client takes the message, and the relay holds one message: a
    /// bridge that reads its goal's feedback only after this returns keeps
    /// the messages that the client is not ready for in the goal's feedback
    /// buffer, whose bound holds a client that falls behind. On a call that
    /// closed it returns at once and drops the message, which has no
    /// reader.
    pub async fn report_feedback(&self, message: impl Into<String>) {
        match &self.surface {
            ActionSurface::Task(task) => task.set_status_message(message),
            ActionSurface::Call { feedback, .. } => {
                let _ = feedback.send(message.into()).await;
            }
        }
    }

    /// Resolves once the client has asked to cancel the goal (immediately,
    /// if it already has): through `tasks/cancel` on a task, by closing the
    /// call otherwise. A task's connection closing is no cancel. Cancellation
    /// is cooperative: the bridge forwards it to the Peppy action's cancel
    /// path and keeps awaiting the terminal result, which decides the
    /// terminal state.
    pub async fn cancel_requested(&self) {
        match &self.surface {
            ActionSurface::Task(task) => task.cancelled().await,
            ActionSurface::Call { closed, .. } => closed.cancelled().await,
        }
    }

    /// Whether the client has asked to cancel the goal: through
    /// `tasks/cancel` on a task, by closing the call otherwise.
    pub fn is_cancel_requested(&self) -> bool {
        match &self.surface {
            ActionSurface::Task(task) => task.is_cancel_requested(),
            ActionSurface::Call { closed, .. } => closed.is_cancelled(),
        }
    }
}

/// One registered action bridge: a validated call (the canonical-JSON goal
/// fields and, on a per-robot surface, the member the goal goes to) in, the
/// canonical JSON of the completed result out, or an [`ActionExit`]
/// describing the non-completed terminal state. Any
/// `Fn(ToolCall, ActionContext) -> impl Future` with those shapes implements
/// it.
///
/// The runtime drops the future of a bridge whose goal outlasts its
/// whole-goal deadline or the TTL of its task, and the futures of every
/// bridge when the endpoint stops. A bridge whose future is dropped before
/// its goal ended stops following the goal and sends its provider a
/// cancel, so no goal runs on with no one following it.
pub trait TaskHandler: Send + Sync + 'static {
    fn start(
        &self,
        call: ToolCall,
        context: ActionContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ActionExit>> + Send>>;
}

impl<F, Fut> TaskHandler for F
where
    F: Fn(ToolCall, ActionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ActionExit>> + Send + 'static,
{
    fn start(
        &self,
        call: ToolCall,
        context: ActionContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ActionExit>> + Send>> {
        Box::pin(self(call, context))
    }
}
