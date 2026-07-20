mod action_poll;
mod confirm;
pub mod container;
pub mod info;
pub mod node;
pub mod platform;
pub mod repo;
pub mod service;
pub mod stack;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::{InstanceState, SerializedNodeGraph};
use peppylib::MessengerHandle;

use crate::{
    context::{AppContext, DaemonConnection},
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
/// is meaningless: render a neutral `-` rather than a stale `healthy`/
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

/// Shared core of the remote-target gates below: refuses a `--core-node`
/// override naming anything but the local daemon. `reason` explains, in terms
/// of the failing command, why its request cannot cross machines; it is built
/// lazily so the happy path allocates nothing.
fn reject_remote_target(
    conn: &DaemonConnection<'_>,
    command: &str,
    reason: impl FnOnce(&str) -> String,
) -> Result<()> {
    if conn.target_core_node != conn.core_node_name {
        return Err(Error::ExecutionFailed(format!(
            "`{command}` does not support --core-node: {} \
             Run the command on that daemon's machine instead.",
            reason(&conn.target_core_node)
        )));
    }
    Ok(())
}

/// Guards the commands whose payload embeds this session's locally resolved
/// messaging endpoint (see [`resolve_messaging_endpoint`]): the daemon does
/// not rewrite `RuntimeConfig`'s `messaging_host`/`messaging_port` server-side
/// (only the macOS container-gateway rewrite), so a runtime config built here
/// for a remote daemon would hand its node an endpoint that only exists on
/// this machine. Until the daemon stamps its own endpoint, these commands
/// refuse a `--core-node` override naming anything but the local daemon.
pub(crate) fn reject_remote_target_for_local_endpoint(
    conn: &DaemonConnection<'_>,
    command: &str,
) -> Result<()> {
    reject_remote_target(conn, command, |target| {
        format!(
            "the runtime config it produces embeds this machine's messaging endpoint, \
             which nodes on daemon '{target}' cannot reach."
        )
    })
}

/// Guards the commands whose request embeds a **caller-local filesystem path**
/// that the daemon resolves on its own machine (e.g. `node init`'s scaffold
/// dir, defaulting to the caller's cwd): sent to a remote daemon, the path
/// would silently be read or created on that machine's filesystem while the
/// CLI reports success with a local-looking path. Until such requests carry no
/// caller-local paths, these commands refuse a `--core-node` override naming
/// anything but the local daemon. Sibling of
/// [`reject_remote_target_for_local_endpoint`].
pub(crate) fn reject_remote_target_for_local_path(
    conn: &DaemonConnection<'_>,
    command: &str,
) -> Result<()> {
    reject_remote_target(conn, command, |target| {
        format!(
            "it operates on a filesystem path from this machine, which would instead \
             be resolved on daemon '{target}''s filesystem."
        )
    })
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
        // Terminal instances have exited, so health is not applicable; the
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

    #[test]
    fn remote_target_gate_rejects_only_a_differing_target() {
        use pmi::MessengerBackend as _;
        block_on(async {
            let mut instance = pmi::MockAdapter::start_router()
                .await
                .expect("mock router should start");
            instance
                .messenger()
                .start_session()
                .await
                .expect("mock session should start");
            let handle = MessengerHandle::from_shared(std::sync::Arc::new(
                tokio::sync::Mutex::new(instance.take_messenger()),
            ));
            let conn = |target: &str| DaemonConnection {
                messenger: &handle,
                core_node_name: "local-daemon".to_string(),
                target_core_node: target.to_string(),
                target_is_override: target != "local-daemon",
                git_hash: "test-git-hash".to_string(),
                shutdown_grace_secs: 5,
                organization_namespace: "local".to_string(),
            };

            // Target == local (the no-override shape): allowed.
            reject_remote_target_for_local_endpoint(&conn("local-daemon"), "peppy node run")
                .expect("a local target must pass the gate");

            // A differing target is refused with an actionable message.
            let err = reject_remote_target_for_local_endpoint(&conn("robot-7"), "peppy node run")
                .expect_err("a remote target must be refused");
            let msg = err.to_string();
            assert!(msg.contains("--core-node"), "names the flag: {msg}");
            assert!(msg.contains("peppy node run"), "names the command: {msg}");
            assert!(msg.contains("robot-7"), "names the target daemon: {msg}");

            // The local-path sibling gate behaves identically: local target
            // passes, a remote target is refused naming flag/command/target.
            reject_remote_target_for_local_path(&conn("local-daemon"), "peppy node init")
                .expect("a local target must pass the path gate");
            let err = reject_remote_target_for_local_path(&conn("robot-7"), "peppy node init")
                .expect_err("a remote target must be refused by the path gate");
            let msg = err.to_string();
            assert!(msg.contains("--core-node"), "names the flag: {msg}");
            assert!(msg.contains("peppy node init"), "names the command: {msg}");
            assert!(msg.contains("robot-7"), "names the target daemon: {msg}");
            Ok(())
        })
        .expect("gate test future resolves");
    }
}
