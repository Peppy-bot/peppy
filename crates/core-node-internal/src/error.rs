use config::failure_kind::PeppyFailureKind;
use peppylib::PeppyError;
use peppylib::messaging::Failure;
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

    /// A cancel stopped a repository refresh between two of its steps: it
    /// read no other repository and published nothing.
    #[error("the repository refresh was cancelled")]
    RefreshCancelled,

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

    // -- datastore
    #[error(
        "instance '{instance_id}' cannot store a datastore key exclusively: the core node runs \
         no process under that id, and an owned key lasts as long as its owner's process. Run \
         the node in the stack with `peppy node run <name>:<tag>` or `peppy stack launch \
         <launcher>`, or store the key with `store`"
    )]
    DatastoreOwnerNotRunning { instance_id: String },

    #[error(
        "datastore key '{key}' is owned by instance '{owner}', which stored it with \
         `store_exclusive`: only that instance stores or removes it until its process ends. Use \
         another key, or stop that instance with {stop}"
    )]
    DatastoreKeyOwned {
        key: String,
        owner: String,
        stop: String,
    },

    #[error(
        "datastore key '{key}' exists with no owner, as `store` writes it: `store_exclusive` \
         takes a key that is absent or that the calling instance already owns. Remove the key with \
         `remove`, or use another key"
    )]
    DatastoreKeyUnowned { key: String },
}

impl Error {
    /// The failure kind a caller reads for this error: a request that does
    /// not decode or names a value the daemon refuses is `rejected`, a
    /// datastore key in the hands of another owner is `precondition_failed`,
    /// a peer that answered with a failure keeps its kind, a peer that is
    /// gone is `resource_unavailable`, a peer that answers too late is
    /// `timeout`, and any other error is `attempt_failed`.
    pub(crate) fn failure_kind(&self) -> PeppyFailureKind {
        match self {
            Self::CoreNodeApi(core_node_api::Error::Encoding(_)) => PeppyFailureKind::AttemptFailed,
            Self::CoreNodeApi(_) | Self::ForbiddenEnvVar(_) | Self::InvalidEnvVar(_) => {
                PeppyFailureKind::Rejected
            }
            Self::DatastoreOwnerNotRunning { .. }
            | Self::DatastoreKeyOwned { .. }
            | Self::DatastoreKeyUnowned { .. } => PeppyFailureKind::PreconditionFailed,
            Self::Peppylib(error) => peppylib_failure_kind(error),
            _ => PeppyFailureKind::AttemptFailed,
        }
    }

    /// The failure a caller reads for this error: its kind and its message.
    pub(crate) fn into_failure(self) -> Failure {
        Failure::new(self.failure_kind(), self.to_string())
    }
}

/// The failure kind of a messaging error that a daemon met while it served a
/// request: a peer that is gone is `resource_unavailable`, a peer that
/// answers too late is `timeout`, and a peer that answered with a failure
/// keeps its kind.
pub(crate) fn peppylib_failure_kind(error: &PeppyError) -> PeppyFailureKind {
    if let Some(failure) = error.failure() {
        return PeppyFailureKind::from_name(failure.kind().as_str())
            .unwrap_or(PeppyFailureKind::AttemptFailed);
    }
    match error {
        PeppyError::ServiceUnreachable { .. }
        | PeppyError::ActionResultUnreachable { .. }
        | PeppyError::ActionFeedbackProducerGone { .. }
        | PeppyError::ActionGoalAbandoned { .. } => PeppyFailureKind::ResourceUnavailable,
        PeppyError::ServiceTimeout { .. } | PeppyError::ActionResultTimeout { .. } => {
            PeppyFailureKind::Timeout
        }
        _ => PeppyFailureKind::AttemptFailed,
    }
}
