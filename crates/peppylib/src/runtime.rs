mod builder;
mod node_runner;
mod processor;

pub use builder::{NodeBuilder, NodeContext, StandaloneConfig};
pub use node_runner::NodeRunner;
pub use processor::Processor;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::task::JoinHandle as TokioJoinHandle;
use tokio_util::sync::CancellationToken as TokioCancellationToken;

/// A handle to a task running in the background.
///
/// This is a wrapper around the underlying runtime's join handle (currently `tokio::task::JoinHandle`).
pub struct TaskHandle<T>(TokioJoinHandle<T>);

impl<T> TaskHandle<T> {
    /// Abort the task associated with the handle.
    ///
    /// The task will be cancelled, and waiting on the handle will return a `JoinError::Cancelled`.
    pub fn abort(&self) {
        self.0.abort();
    }

    /// Returns `true` if the task has finished.
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl<T> Future for TaskHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|res| res.map_err(JoinError))
    }
}

impl<T> From<TokioJoinHandle<T>> for TaskHandle<T> {
    fn from(handle: TokioJoinHandle<T>) -> Self {
        Self(handle)
    }
}

impl<T> fmt::Debug for TaskHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TaskHandle").finish()
    }
}

/// An error that might occur when waiting for a task to complete.
#[derive(Debug)]
pub struct JoinError(tokio::task::JoinError);

impl JoinError {
    /// Returns true if the task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Returns true if the task panicked.
    pub fn is_panic(&self) -> bool {
        self.0.is_panic()
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for JoinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// A token that can be used to signal cancellation to one or more tasks.
///
/// This is a wrapper around `tokio_util::sync::CancellationToken`.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(TokioCancellationToken);

impl CancellationToken {
    /// Create a new `CancellationToken`.
    pub fn new() -> Self {
        Self(TokioCancellationToken::new())
    }

    /// Cancel the token and notify all tasks waiting on it.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Returns `true` if the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Waits until the token is cancelled.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }

    /// Creates a child token that is cancelled when the parent is cancelled.
    pub fn child_token(&self) -> Self {
        Self(self.0.child_token())
    }
}

impl From<TokioCancellationToken> for CancellationToken {
    fn from(token: TokioCancellationToken) -> Self {
        Self(token)
    }
}

/// Spawns a new asynchronous task.
pub fn spawn<T>(future: T) -> TaskHandle<T::Output>
where
    T: Future + Send + 'static,
    T::Output: Send + 'static,
{
    TaskHandle(tokio::spawn(future))
}

#[cfg(test)]
mod tests {
    use super::CancellationToken;

    #[test]
    fn child_token_is_cancelled_when_parent_is_cancelled() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        assert!(!child.is_cancelled());

        parent.cancel();
        assert!(
            child.is_cancelled(),
            "cancelling the parent must propagate to the child"
        );
    }

    #[test]
    fn child_token_cancel_does_not_cancel_parent() {
        let parent = CancellationToken::new();
        let child = parent.child_token();

        child.cancel();
        assert!(child.is_cancelled());
        assert!(
            !parent.is_cancelled(),
            "cancelling a child must not cancel its parent"
        );
    }
}
