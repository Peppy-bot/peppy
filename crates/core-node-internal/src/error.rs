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
    InvalidSenderTarget(#[from] pmi::SenderTargetError),

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

    // -- lifecycle
    #[error("Apptainer pre-flight check failed:\n\n{0}")]
    RuntimeCheck(String),

    #[error("core node already started")]
    AlreadyStarted,

    #[error(
        "core node name '{name}' is already in use by another daemon reachable over this \
         router/federation; refusing to start. Set `core_node_name` in \
         `~/.peppy/conf/peppy_config.json5` (or run `peppy service serve --core-node-name`) \
         to give this daemon a unique name, then restart it"
    )]
    CoreNodeNameTaken { name: String },

    #[error(
        "cannot verify core node name '{name}' is free: liveliness queries went \
         {blind_for:.1?} without reporting this daemon's own claim candidacy, so the broker \
         view is degraded (queries timing out or replies dropped, typically an overloaded or \
         unreachable router); refusing to commit the name blind. Restart the daemon once the \
         router is responsive"
    )]
    NameClaimUnverifiable {
        name: String,
        blind_for: std::time::Duration,
    },
}
