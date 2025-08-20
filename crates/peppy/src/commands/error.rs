use thiserror::Error;

#[derive(Error, Debug)]
pub enum CommandError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Node command error: {0}")]
    Node(#[from] super::node::error::NodeCommandError),
    
    #[error("Command execution failed: {0}")]
    ExecutionFailed(String),
}