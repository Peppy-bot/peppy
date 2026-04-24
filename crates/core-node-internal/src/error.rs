use peppylib::PeppyError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),

    #[error(transparent)]
    Peppylib(#[from] PeppyError),

    #[error(transparent)]
    CoreNodeApi(#[from] core_node_api::Error),

    #[error("task join failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("forbidden environment variable '{0}' is not allowed")]
    ForbiddenEnvVar(String),

    #[error("invalid environment variable: {0}")]
    InvalidEnvVar(String),

    // -- generator-internal
    #[error(transparent)]
    GeneratorError(#[from] generator::GeneratorError),

    // -- templates
    #[error("template rendering error: {0}")]
    Template(#[from] askama::Error),

    // -- repository operations
    #[error("duplicate repository id {id} in {file}")]
    DuplicateRepoId { id: u64, file: String },

    // -- node operations
    #[error("failed to shutdown node instance '{instance_id}': {reason}")]
    ShutdownInstanceFailed { instance_id: String, reason: String },
}
