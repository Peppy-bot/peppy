use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum FileEvent {
    NodeConfigCreated(PathBuf),
    NodeConfigModified(PathBuf),
    NodeConfigDeleted(PathBuf),
}
