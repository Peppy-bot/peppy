//! Action-backed MCP tasks: the handler contract an action bridge
//! implements and the context the runtime hands it while a goal runs.
//!
//! The runtime owns the whole MCP side of a task (creation, confirmation,
//! polling, cancellation intent, the deadline, terminal mapping); the bridge
//! owns the whole Peppy side (firing the goal, draining feedback, forwarding
//! the cancel, awaiting the result). [`ActionContext`] is the seam between
//! the two.

use rmcp::task_manager::TaskContext;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// How an action bridge finished without a completed result. The runtime
/// maps it onto the MCP task's terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExit {
    /// The Peppy action ended cancelled; the MCP task settles as
    /// `cancelled`.
    Cancelled,
    /// The goal could not run to completion (rejected, abandoned, expired,
    /// or a transport failure); the MCP task settles as `failed` with this
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

/// The runtime-side surface an action bridge drives while its goal runs.
#[derive(Clone)]
pub struct ActionContext {
    pub(crate) inner: TaskContext,
}

impl ActionContext {
    /// Publishes a feedback message as the task's status message;
    /// `tasks/get` reports the latest one.
    pub fn report_feedback(&self, message: impl Into<String>) {
        self.inner.set_status_message(message);
    }

    /// Resolves once the client has requested cancellation via
    /// `tasks/cancel` (immediately, if it already has). Cancellation is
    /// cooperative on both sides: the bridge forwards it to the Peppy
    /// action's cancel path and keeps awaiting the terminal result, which
    /// decides the task's terminal state.
    pub async fn cancel_requested(&self) {
        self.inner.cancelled().await;
    }

    /// Whether `tasks/cancel` has been received for this task.
    pub fn is_cancel_requested(&self) -> bool {
        self.inner.is_cancel_requested()
    }
}

/// One registered action bridge: validated canonical-JSON goal fields in,
/// the canonical JSON of the completed result out, or an [`ActionExit`]
/// describing the non-completed terminal state. Any
/// `Fn(Value, ActionContext) -> impl Future` with those shapes implements
/// it.
pub trait TaskHandler: Send + Sync + 'static {
    fn start(
        &self,
        input: Value,
        context: ActionContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ActionExit>> + Send>>;
}

impl<F, Fut> TaskHandler for F
where
    F: Fn(Value, ActionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ActionExit>> + Send + 'static,
{
    fn start(
        &self,
        input: Value,
        context: ActionContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ActionExit>> + Send>> {
        Box::pin(self(input, context))
    }
}
