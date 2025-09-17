use std::path::{Path, PathBuf};

use config::NodeIndexState;
use tokio::sync::broadcast;

pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

pub struct AppContext {
    broadcaster: broadcast::Sender<AppEvent>,
    pub root_dir: PathBuf,
}

impl AppContext {
    pub fn new(channel_capacity: usize, root_dir: impl AsRef<Path>) -> Self {
        let (broadcaster, _) = broadcast::channel(channel_capacity);
        Self {
            broadcaster,
            root_dir: PathBuf::from(root_dir.as_ref()),
        }
    }

    pub fn event_sender(&self) -> broadcast::Sender<AppEvent> {
        self.broadcaster.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.broadcaster.subscribe()
    }
}

impl Default for AppContext {
    fn default() -> Self {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        Self::new(DEFAULT_CHANNEL_CAPACITY, root_dir)
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum AppEvent {
    NodeConfigChanged(NodeIndexState),
    Shutdown,
    Custom {
        kind: String,
        payload: Option<String>,
    },
}
