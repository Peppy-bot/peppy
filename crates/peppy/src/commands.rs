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

use core_node_api::{InstanceState, SerializedNodeGraph};
use peppylib::MessengerHandle;

use crate::{
    context::AppContext,
    error::{Error, Result},
};

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

/// The health cell for an instance, accounting for its lifecycle state. A
/// terminal instance (`Finished`/`Failed`) has exited, so its last health probe
/// is meaningless — render a neutral `-` rather than a stale `healthy`/
/// `unhealthy`. Live instances render their probed health via [`health_label`].
/// Shared by `stack list` and `node info` so the two never diverge.
pub(crate) fn instance_health_label(state: InstanceState, healthy: bool) -> &'static str {
    if state.is_terminal() {
        "-"
    } else {
        health_label(healthy)
    }
}

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()>;
}

/// Parses the daemon's serialized stack graph from its JSON payload. Single
/// owner of the parse and its error message so the commands that read the stack
/// snapshot (stack list, node run, node remove, node runtime-config) cannot
/// drift on it.
pub(crate) fn parse_stack_graph(graph_json: &str) -> Result<SerializedNodeGraph> {
    serde_json::from_str(graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))
}

/// Resolves the messaging host and port a node should connect to, falling back
/// to the default host plus the messenger's port when the endpoint is not
/// directly advertised. Shared by `node run` and `node runtime-config`.
pub(crate) async fn resolve_messaging_endpoint(messenger: &MessengerHandle) -> (String, u16) {
    match messenger.messaging_endpoint().await {
        Some(endpoint) => endpoint,
        None => (
            config::consts::DEFAULT_MESSAGING_HOST.to_string(),
            messenger.messaging_port().await,
        ),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_label_is_the_single_source_of_truth() {
        // `stack list` and `node info` both render this; pin it so they cannot
        // drift apart.
        assert_eq!(health_label(true), "healthy");
        assert_eq!(health_label(false), "unhealthy");
    }

    #[test]
    fn instance_health_label_neutralizes_terminal_states() {
        // Live instances report their probed health.
        assert_eq!(
            instance_health_label(InstanceState::Running, true),
            "healthy"
        );
        assert_eq!(
            instance_health_label(InstanceState::Starting, false),
            "unhealthy"
        );
        // Terminal instances have exited, so health is not applicable — the
        // stale `healthy` flag must never surface as a verdict.
        assert_eq!(instance_health_label(InstanceState::Finished, true), "-");
        assert_eq!(instance_health_label(InstanceState::Finished, false), "-");
        assert_eq!(instance_health_label(InstanceState::Failed, true), "-");
        assert_eq!(instance_health_label(InstanceState::Failed, false), "-");
    }

    #[test]
    fn block_on_runs_a_future_without_an_ambient_runtime() {
        // The no-current-handle branch builds a fresh runtime.
        let value = block_on(async { Ok::<_, crate::error::Error>(7) }).expect("future resolves");
        assert_eq!(value, 7);
    }
}
