use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard};

use config::NodeIndexState;
use node_stack::NodeStack;

pub struct MasterContext {
    pub root_dir: PathBuf,
    node_stack: RwLock<NodeStack>,
}

impl MasterContext {
    pub fn new(root_dir: impl AsRef<Path>, node_stack: NodeStack) -> Self {
        let (broadcaster, _) = broadcast::channel(channel_capacity);
        Self {
            root_dir: PathBuf::from(root_dir.as_ref()),
            node_stack: RwLock::new(node_stack),
        }
    }

    pub fn node_stack(&self) -> RwLockReadGuard<'_, NodeStack> {
        self.node_stack.read().expect("node stack lock poisoned")
    }

    pub fn set_node_stack(&self, node_stack: NodeStack) {
        *self.node_stack.write().expect("node stack lock poisoned") = node_stack;
    }
}

impl Default for MasterContext {
    fn default() -> Self {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let node_stack = NodeStack::new(); // TODO: The node stack should contain the master node itself by default
        Self::new(root_dir, node_stack)
    }
}
