use config::NodeConfig;
use std::path::PathBuf;

pub enum NodeSource {
    Network,
    Filesystem,
}

// An entity that holds a Node and its config under a single object
pub struct Node {
    pub source: NodeSource,
    pub config: NodeConfig,
}

#[derive(Debug, Clone)]
pub enum FileEvent {
    NodeConfigCreated(PathBuf),
    NodeConfigModified(PathBuf),
    NodeConfigDeleted(PathBuf),
}

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    ExternalNodeDetected { uri: String },
}

// Define a unified Event enum for all aggregated events
#[derive(Debug)]
pub enum NodeDetectionEvent {
    FileEvent(FileEvent),
    NetworkEvent(NetworkEvent),
}
