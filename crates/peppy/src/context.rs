use crate::daemon_state::DaemonState;
use crate::error::Error;
use peppylib::MessengerHandle;
use pmi::Messenger;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

pub struct AppContext {
    pub root_dir: PathBuf,
    daemon_state_path: Option<PathBuf>,
    messenger_handle: OnceCell<MessengerHandle>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            daemon_state_path: None,
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
                    .namespace(namespace)
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
    pub core_node_name: String,
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
        Ok(DaemonConnection {
            messenger,
            core_node_name: daemon_state.core_node_name,
            git_hash: daemon_state.git_hash,
            shutdown_grace_secs: daemon_state.shutdown_grace_secs,
            organization_namespace: daemon_state.organization_namespace,
        })
    }
}
