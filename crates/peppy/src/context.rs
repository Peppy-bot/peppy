use crate::daemon_state::DaemonState;
use peppylib::MessengerHandle;
use pmi::Messenger;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

pub struct AppContext {
    pub root_dir: PathBuf,
    messenger_handle: OnceCell<MessengerHandle>,
}

impl AppContext {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            messenger_handle: OnceCell::new(),
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
