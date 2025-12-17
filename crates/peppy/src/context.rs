use std::path::{Path, PathBuf};

use peppylib::MessengerHandle;
use tokio::sync::OnceCell;

use crate::error::Result;

const DEFAULT_ZENOH_HOST: &str = "127.0.0.1";

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

    pub async fn connect(&self) -> Result<()> {
        self.messenger_handle
            .get_or_try_init(|| async {
                MessengerHandle::from_host_port(
                    DEFAULT_ZENOH_HOST,
                    config::consts::DEFAULT_ZENOH_PORT,
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
