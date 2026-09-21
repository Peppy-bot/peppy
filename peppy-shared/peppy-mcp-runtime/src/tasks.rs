//! Action-backed tools: the handler contract an action bridge implements
//! and the context the runtime hands it while a goal runs.
//!
//! The runtime owns the whole MCP side of an action (the task or the call
//! it runs in, confirmation, feedback delivery, cancellation intent, the
//! deadline, terminal mapping); the bridge owns the whole Peppy side
//! (firing the goal, draining feedback, forwarding the cancel, awaiting the
//! result). [`ActionContext`] is the seam between the two, and it hides
//! which of the two MCP surfaces the goal runs on.

use crate::server::ToolCall;
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
    /// The Peppy action ended cancelled; an MCP task settles as
    /// `cancelled`.
    Cancelled,
    /// The goal could not run to completion (rejected, abandoned, expired,
    /// or a transport failure); an MCP task settles as `failed` with this
    /// message.
    Failed(String),
}

impl std::fmt::Display for ActionExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "the action was cancelled"),
            Self::Failed(detail) => write!(f, "the action failed: {detail}"),
        }
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
    /// call, and the client closing the call is the cancel signal.
    Call {
        feedback: mpsc::UnboundedSender<String>,
        cancel: CancellationToken,
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
    pub fn report_feedback(&self, message: impl Into<String>) {
        match &self.surface {
            ActionSurface::Task(task) => task.set_status_message(message),
            // A closed receiver means the call already settled; feedback
            // after that has no reader.
            ActionSurface::Call { feedback, .. } => {
                let _ = feedback.send(message.into());
            }
        }
    }

    /// Resolves once the client has requested cancellation (immediately, if
    /// it already has): through `tasks/cancel` on a task, by closing the
    /// call otherwise. Cancellation is cooperative on both sides: the bridge
    /// forwards it to the Peppy action's cancel path and keeps awaiting the
    /// terminal result, which decides the terminal state.
    pub async fn cancel_requested(&self) {
        match &self.surface {
            ActionSurface::Task(task) => task.cancelled().await,
            ActionSurface::Call { cancel, .. } => cancel.cancelled().await,
        }
    }

    /// Whether the client has requested cancellation of this goal.
    pub fn is_cancel_requested(&self) -> bool {
        match &self.surface {
            ActionSurface::Task(task) => task.is_cancel_requested(),
            ActionSurface::Call { cancel, .. } => cancel.is_cancelled(),
        }
    }
}

/// One registered action bridge: a validated call (the canonical-JSON goal
/// fields and, on a per-robot surface, the member the goal goes to) in, the
/// canonical JSON of the completed result out, or an [`ActionExit`]
/// describing the non-completed terminal state. Any
/// `Fn(ToolCall, ActionContext) -> impl Future` with those shapes implements
/// it.
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
