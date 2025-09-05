use std::path::PathBuf;

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
