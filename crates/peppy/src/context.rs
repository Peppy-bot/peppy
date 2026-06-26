use crate::auth::{http::HttpClient, router, storage};
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
    /// Handle to the caller's *remote* per-user zenoh router. A distinct
    /// connection target from [`messenger_handle`](Self::messenger_handle) (the
    /// local daemon), so it gets its own cell — the two are not interchangeable.
    router_handle: OnceCell<MessengerHandle>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            daemon_state_path: None,
            messenger_handle: OnceCell::new(),
            router_handle: OnceCell::new(),
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
            router_handle: OnceCell::new(),
        }
    }

    async fn connect_with_port(&self, messaging_port: u16) -> crate::error::Result<()> {
        self.messenger_handle
            .get_or_try_init(|| async {
                MessengerHandle::from_host_port(
                    config::consts::DEFAULT_MESSAGING_HOST,
                    messaging_port,
                )
                .await
            })
            .await?;
        Ok(())
    }

    pub fn messenger_handle(&self) -> Option<&MessengerHandle> {
        self.messenger_handle.get()
    }

    /// Connects to the caller's *remote* per-user zenoh router over `tls/`,
    /// pulling (and caching) the connection config from the backend. Reuses a
    /// cached config while fresh; otherwise re-pulls, refreshing the access
    /// token on a `401`. The remote counterpart to
    /// [`connect_to_daemon`](Self::connect_to_daemon): a distinct target dialed
    /// over an end-to-end-encrypted link, validated against the deployment CA
    /// (`PEPPY_ROUTER_CA_CERT`) with name verification on.
    ///
    /// `api_url` is resolved by the caller (the flag/env/`resource_servers`
    /// precedence the auth commands use). Cached behind its own cell, so the
    /// first call provisions/dials and later calls return the live handle.
    pub async fn connect_to_router(&self, api_url: &str) -> crate::error::Result<&MessengerHandle> {
        self.router_handle
            .get_or_try_init(|| async {
                let creds_path = storage::default_path();
                let http = HttpClient::new();
                let pat = std::env::var("PEPPY_API_KEY")
                    .ok()
                    .filter(|v| !v.is_empty());
                // Blocking HTTP pull (ureq), consistent with the rest of the
                // auth engine; it is a single quick request and the connect path
                // has nothing else scheduled meanwhile.
                let endpoint = router::resolve_router_endpoint(
                    &creds_path,
                    &http,
                    api_url,
                    pat,
                    router::ca_from_env(),
                )?;
                MessengerHandle::from_remote_tls(&endpoint.host, endpoint.port, endpoint.tls)
                    .await
                    .map_err(Error::from)
            })
            .await
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
}

impl AppContext {
    pub(crate) async fn connect_to_daemon(&self) -> crate::error::Result<DaemonConnection<'_>> {
        let daemon_state = self.read_daemon_state()?;
        self.connect_with_port(daemon_state.messaging_port).await?;
        let messenger = self
            .messenger_handle()
            .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;
        Ok(DaemonConnection {
            messenger,
            core_node_name: daemon_state.core_node_name,
            git_hash: daemon_state.git_hash,
            shutdown_grace_secs: daemon_state.shutdown_grace_secs,
        })
    }
}
