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
    /// In-process test emulations root their daemon at a per-test temp dir;
    /// this points the context at that daemon's state file without mutating
    /// the process-wide environment, which is not safe from parallel tests.
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

    /// The `--core-node` target, when one was given. Read by command groups
    /// that cannot honor it, so they can refuse rather than silently ignore it.
    pub(crate) fn core_node_override(&self) -> Option<&str> {
        self.core_node_override.as_deref()
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

    async fn connect_with_endpoint(
        &self,
        messaging_host: &str,
        messaging_port: u16,
        namespace: config::namespace::Namespace,
    ) -> crate::error::Result<()> {
        // Open the control session under the typed namespace recorded by the
        // daemon generation so the CLI reaches its daemon and node services.
        self.messenger_handle
            .get_or_try_init(|| async {
                MessengerHandle::connect(messaging_host, messaging_port)
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
    /// Whether `target_core_node` came from an explicit `--core-node`
    /// override. This stays distinct from comparing the two names because an
    /// explicit override may name the local daemon; `stack list` must still
    /// honor that as a single-target request.
    pub target_is_override: bool,
    pub git_hash: String,
    /// Cooperative-shutdown grace the daemon will wait before force-killing a
    /// node, from its `peppy_config`. Lets `node stop` size its request timeout
    /// to outlast the daemon's grace + reap window.
    pub shutdown_grace_secs: u64,
    /// The workspace namespace recorded by the generation this connection was
    /// established against, captured from the *same* `DaemonState` read the
    /// connection used. Callers reuse this instead of reading the state again,
    /// which could race a restart and pair this connection's data with a different
    /// generation's namespace.
    pub namespace: config::namespace::Namespace,
}

impl AppContext {
    pub(crate) async fn connect_to_daemon(&self) -> crate::error::Result<DaemonConnection<'_>> {
        let daemon_state = self.read_daemon_state()?;
        self.connect_with_endpoint(
            &daemon_state.messaging_host,
            daemon_state.messaging_port,
            daemon_state.namespace.clone(),
        )
        .await?;
        let messenger = self
            .messenger_handle()
            .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;
        let target_is_override = self.core_node_override.is_some();
        let target_core_node = self
            .core_node_override
            .clone()
            .unwrap_or_else(|| daemon_state.core_node_name.clone());
        Ok(DaemonConnection {
            messenger,
            core_node_name: daemon_state.core_node_name,
            target_core_node,
            target_is_override,
            git_hash: daemon_state.git_hash,
            shutdown_grace_secs: daemon_state.shutdown_grace_secs,
            namespace: daemon_state.namespace,
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
            config::consts::DEFAULT_MESSAGING_HOST,
            0,
            "test-git-hash",
            config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            config::namespace::Namespace::local(),
            None,
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
            assert!(!conn.target_is_override);
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
            assert!(conn.target_is_override);
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
            assert!(!conn.target_is_override);
            Ok(())
        })
        .expect("connecting should succeed");
    }

    #[test]
    fn an_explicit_local_name_is_still_an_override() {
        crate::commands::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let (ctx, _router) = context_with_state(dir.path()).await;
            let ctx = ctx.with_core_node_override(Some("local-daemon".to_string()));
            let conn = ctx.connect_to_daemon().await?;
            assert_eq!(conn.target_core_node, conn.core_node_name);
            assert!(conn.target_is_override);
            Ok(())
        })
        .expect("connecting should succeed");
    }
}
