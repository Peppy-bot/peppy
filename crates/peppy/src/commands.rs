mod action_poll;
mod confirm;
pub mod container;
pub mod info;
pub mod node;
pub mod repo;
pub mod service;
pub mod stack;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::{context::AppContext, error::Result};

/// Instance ID used by the CLI when communicating with the daemon.
pub(crate) const CALLER_INSTANCE_ID: &str = "peppy-cli";

/// Timeout for action goals to be accepted by the daemon (should be fast).
pub(crate) const GOAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of lines to display in the scrolling output region.
pub(crate) const SCROLLING_OUTPUT_LINES: usize = 10;

/// Single source of truth for the word `stack list` and `node info` print for
/// an instance's health, so the two commands can never drift apart on it.
pub(crate) fn health_label(healthy: bool) -> &'static str {
    if healthy { "healthy" } else { "unhealthy" }
}

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()>;
}

pub(crate) fn block_on<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(future)
        }
    }
}
