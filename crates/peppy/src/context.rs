use std::path::PathBuf;

use tokio::sync::broadcast;

const DEFAULT_CHANNEL_CAPACITY: usize = 64;

pub struct AppContext {
    broadcaster: broadcast::Sender<AppEvent>,
}

impl AppContext {
    pub fn new(channel_capacity: usize) -> Self {
        let (broadcaster, _) = broadcast::channel(channel_capacity);
        Self { broadcaster }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CHANNEL_CAPACITY)
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
        Self::with_default_capacity()
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum AppEvent {
    NodeConfigChanged(PathBuf),
    Shutdown,
    Custom {
        kind: String,
        payload: Option<String>,
    },
}
