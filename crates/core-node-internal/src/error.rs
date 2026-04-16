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

    #[error("task join failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("capnp encoding error: {0}")]
    Capnp(#[from] capnp::Error),

    #[error("capnp schema error: {0}")]
    CapnpNotInSchema(#[from] capnp::NotInSchema),

    #[error("invalid UTF-8 in message: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("decoding error: {0}")]
    Decoding(String),

    #[error("encoding error: {0}")]
    Encoding(String),

    #[error("forbidden environment variable '{0}' is not allowed")]
    ForbiddenEnvVar(String),

    #[error("invalid environment variable: {0}")]
    InvalidEnvVar(String),

    #[error("RepoNode is not a valid variant source")]
    RepoNodeNotValidVariantSource,

    // -- generator-internal
    #[error(transparent)]
    GeneratorError(#[from] generator::GeneratorError),

    // -- config parsing
    #[error(transparent)]
    ParsingError(#[from] config::ParsingError),

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
