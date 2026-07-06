use crate::error::Error;
use daemon::state::DaemonState;
use peppylib::{MessengerHandle, SessionScope};
use pmi::Messenger;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

pub struct AppContext {
    pub root_dir: PathBuf,
    daemon_state_path: Option<PathBuf>,
    /// `--core-node` override: the core node daemon-scoped commands address
    /// instead of the local daemon. `None` targets the local daemon.
    core_node_override: Option<String>,
    messenger_handle: OnceCell<MessengerHandle>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            daemon_state_path: None,
            core_node_override: None,
            messenger_handle: OnceCell::new(),
        }
    }

    /// Builds a context rooted at the process's current working directory.
    ///
    /// Fallible because reading the cwd is a syscall that can fail (a deleted or
    /// unreadable directory, some sandboxes). The failure flows through the
    /// crate error type so the CLI reports it and exits cleanly, rather than
    /// panicking from a `Default` impl.
    pub fn from_current_dir() -> crate::error::Result<Self> {
        let root_dir = std::env::current_dir()?;
        Ok(Self::new(root_dir))
    }

    /// Overrides the daemon state file path for this context.
    ///
    /// This avoids relying on the process-wide `PEPPY_DAEMON_STATE_FILE` env var, which is not
    /// safe to mutate from parallel tests.
    pub fn with_daemon_state_file(mut self, daemon_state_path: impl AsRef<Path>) -> Self {
        self.daemon_state_path = Some(daemon_state_path.as_ref().to_path_buf());
        self
    }

    /// Sets the `--core-node` override: daemon-scoped commands will address
    /// the named core node instead of the local daemon. `None` keeps the
    /// default (the local daemon). Only the *target* of each request changes;
    /// the CLI still connects to the local daemon's router, whose federation
    /// carries the traffic to the remote core node.
    pub fn with_core_node_override(mut self, core_node: Option<String>) -> Self {
        self.core_node_override = core_node;
        self
    }

    pub(crate) fn read_daemon_state(&self) -> crate::error::Result<DaemonState> {
        let state = match &self.daemon_state_path {
            Some(path) => DaemonState::read_from(path),
            None => DaemonState::read(),
        }
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to read daemon state. Is the peppy daemon running? Error: {}",
                e
            ))
        })?;
        Ok(state)
    }

    pub fn core_node_name(&self) -> crate::error::Result<String> {
        Ok(self.read_daemon_state()?.core_node_name)
    }

    /// Creates an AppContext with a pre-initialized messenger handle.
    /// This is useful for testing with a shared mock messenger.
    ///
    /// The cell is built already populated, so injecting the handle cannot fail
    /// and there is no post-construction `set` whose error would have to be
    /// discarded.
    pub fn with_messenger(root_dir: impl AsRef<Path>, messenger: Arc<Mutex<Messenger>>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            daemon_state_path: None,
            core_node_override: None,
            messenger_handle: OnceCell::new_with(Some(MessengerHandle::from_shared(messenger))),
        }
    }

    async fn connect_with_port(
        &self,
        messaging_port: u16,
        organization_namespace: &str,
    ) -> crate::error::Result<()> {
        // Open the control session under the daemon's namespace so the CLI reaches
        // the daemon/node services that run under it. The daemon recorded the
        // namespace in `DaemonState` before binding the control socket, so it is a
        // valid value; resolve defensively (a bad value falls back to `local`).
        let namespace = config::org::resolve_session_namespace(Some(organization_namespace));
        self.messenger_handle
            .get_or_try_init(|| async {
                MessengerHandle::connect(config::consts::DEFAULT_MESSAGING_HOST, messaging_port)
                    .scope(SessionScope::Namespace(namespace))
                    .await
            })
            .await?;
        Ok(())
    }

    pub fn messenger_handle(&self) -> Option<&MessengerHandle> {
        self.messenger_handle.get()
    }
}

pub(crate) struct DaemonConnection<'a> {
    pub messenger: &'a MessengerHandle,
    /// The local daemon's core-node name: the caller identity every request
    /// rides under (`bound_core_node` / `as_core_node`). Always local, even
    /// when a `--core-node` override redirects the target.
    pub core_node_name: String,
    /// The core node commands address: the `--core-node` override when one
    /// was given, else the local daemon ([`Self::core_node_name`]). Only the
    /// *target* of a request follows the override — the connection still
    /// dials the local daemon's router and the caller identity stays local.
    pub target_core_node: String,
    pub git_hash: String,
    /// Cooperative-shutdown grace the daemon will wait before force-killing a
    /// node, from its `peppy_config`. Lets `node stop` size its request timeout
    /// to outlast the daemon's grace + reap window.
    pub shutdown_grace_secs: u64,
    /// The organization namespace recorded by the generation this connection was
    /// established against, captured from the *same* `DaemonState` read the
    /// connection used. Callers reuse this instead of reading the state again,
    /// which could race a restart and pair this connection's data with a different
    /// generation's namespace.
    pub organization_namespace: String,
}

impl AppContext {
    pub(crate) async fn connect_to_daemon(&self) -> crate::error::Result<DaemonConnection<'_>> {
        let daemon_state = self.read_daemon_state()?;
        self.connect_with_port(
            daemon_state.messaging_port,
            &daemon_state.organization_namespace,
        )
        .await?;
        let messenger = self
            .messenger_handle()
            .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;
        let target_core_node = self
            .core_node_override
            .clone()
            .unwrap_or_else(|| daemon_state.core_node_name.clone());
        Ok(DaemonConnection {
            messenger,
            core_node_name: daemon_state.core_node_name,
            target_core_node,
            git_hash: daemon_state.git_hash,
            shutdown_grace_secs: daemon_state.shutdown_grace_secs,
            organization_namespace: daemon_state.organization_namespace,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmi::{MessengerBackend, MockAdapter, MockInstance};

    /// Builds an `AppContext` wired to a mock messenger and a daemon state
    /// file recording `local-daemon` as the local core-node name. The
    /// returned [`MockInstance`] keeps the mock router alive; the connection
    /// tests never exchange messages, but the guard makes that explicit.
    async fn context_with_state(dir: &Path) -> (AppContext, MockInstance) {
        let mut instance = MockAdapter::start_router()
            .await
            .expect("mock router should start");
        instance
            .messenger()
            .start_session()
            .await
            .expect("mock session should start");
        let messenger = Arc::new(Mutex::new(instance.take_messenger()));

        let state_path = dir.join("daemon_state.json5");
        let state = DaemonState::new(
            "local-daemon",
            0,
            "test-git-hash",
            config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            "local",
        );
        DaemonState::write_to(&state_path, &state).expect("daemon state should write");

        let ctx = AppContext::with_messenger(dir, messenger).with_daemon_state_file(&state_path);
        (ctx, instance)
    }

    #[test]
    fn target_core_node_defaults_to_the_local_daemon() {
        crate::commands::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (ctx, _router) = context_with_state(dir.path()).await;
            let conn = ctx.connect_to_daemon().await?;
            assert_eq!(conn.core_node_name, "local-daemon");
            assert_eq!(
                conn.target_core_node, "local-daemon",
                "without --core-node the target is the local daemon"
            );
            Ok(())
        })
        .expect("connecting without an override should succeed");
    }

    #[test]
    fn core_node_override_redirects_only_the_target() {
        crate::commands::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (ctx, _router) = context_with_state(dir.path()).await;
            let ctx = ctx.with_core_node_override(Some("robot-7".to_string()));
            let conn = ctx.connect_to_daemon().await?;
            // The caller identity stays the local daemon; only the target moves.
            assert_eq!(conn.core_node_name, "local-daemon");
            assert_eq!(conn.target_core_node, "robot-7");
            Ok(())
        })
        .expect("connecting with an override should succeed");
    }

    #[test]
    fn a_none_override_is_the_default() {
        crate::commands::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (ctx, _router) = context_with_state(dir.path()).await;
            let ctx = ctx.with_core_node_override(None);
            let conn = ctx.connect_to_daemon().await?;
            assert_eq!(conn.target_core_node, conn.core_node_name);
            Ok(())
        })
        .expect("connecting should succeed");
    }
}
