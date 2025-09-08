use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum NodeConfigEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}
