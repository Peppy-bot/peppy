use std::path::{Path, PathBuf};
use std::sync::RwLock;

use config::NodeIndexState;
use node_stack::NodeStack;
use tokio::sync::broadcast;

pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

pub struct AppContext {
    pub root_dir: PathBuf,
    node_stack: RwLock<Option<NodeStack>>,
    broadcaster: broadcast::Sender<AppEvent>,
}

impl AppContext {
    pub fn new(channel_capacity: usize, root_dir: impl AsRef<Path>) -> Self {
        let (broadcaster, _) = broadcast::channel(channel_capacity);
        Self {
            broadcaster,
            root_dir: PathBuf::from(root_dir.as_ref()),
            node_stack: RwLock::new(None),
        }
    }

    pub fn event_sender(&self) -> broadcast::Sender<AppEvent> {
        self.broadcaster.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.broadcaster.subscribe()
    }

    pub fn node_stack(&self) -> Option<NodeStack> {
        self.node_stack
            .read()
            .expect("node stack lock poisoned")
            .clone()
    }

    pub fn set_node_stack(&self, node_stack: NodeStack) {
        *self.node_stack.write().expect("node stack lock poisoned") = Some(node_stack);
    }

    pub fn reset_node_stack(&self) {
        *self.node_stack.write().expect("node stack lock poisoned") = None;
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
