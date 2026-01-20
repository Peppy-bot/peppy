use crate::daemon_state::DaemonState;
use peppylib::MessengerHandle;
use pmi::Messenger;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

pub struct AppContext {
    pub root_dir: PathBuf,
    messenger_handle: OnceCell<MessengerHandle>,
    /// Optional pre-loaded daemon state (used in tests to avoid file-based state)
    daemon_state: Option<DaemonState>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            messenger_handle: OnceCell::new(),
            daemon_state: None,
        }
    }

    /// Creates an AppContext with a pre-initialized messenger handle.
    /// This is useful for testing with a shared mock messenger.
    pub fn with_messenger(root_dir: impl AsRef<Path>, messenger: Arc<Mutex<Messenger>>) -> Self {
        let ctx = Self::new(root_dir);
        let _ = ctx
            .messenger_handle
            .set(MessengerHandle::from_shared(messenger));
        ctx
    }

    /// Creates an AppContext with a pre-initialized messenger handle and daemon state.
    /// This is the recommended constructor for tests as it avoids file-based state
    /// and enables proper test isolation.
    pub fn with_messenger_and_state(
        root_dir: impl AsRef<Path>,
        messenger: Arc<Mutex<Messenger>>,
        daemon_state: DaemonState,
    ) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            messenger_handle: {
                let cell = OnceCell::new();
                let _ = cell.set(MessengerHandle::from_shared(messenger));
                cell
            },
            daemon_state: Some(daemon_state),
        }
    }

    /// Returns the daemon state, either from the pre-loaded state or by reading from disk.
    pub fn daemon_state(&self) -> std::io::Result<DaemonState> {
        if let Some(state) = &self.daemon_state {
            Ok(state.clone())
        } else {
            DaemonState::read()
        }
    }

    pub async fn connect(&self) -> crate::error::Result<()> {
        self.messenger_handle
            .get_or_try_init(|| async {
                let messaging_port = DaemonState::read()?.messaging_port;
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
}

impl Default for AppContext {
    fn default() -> Self {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        Self::new(root_dir)
    }
}
