use config::node::InterfaceKind;
use peppylib::PeppyError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum DependencyInterfaceIntegrityError {
    #[error("dependency `{dependency}:{dependency_tag}` is not present in the node stack")]
    DependencyNotInStack {
        dependency: String,
        dependency_tag: String,
    },

    #[error("failed to compute interface integrity for `{dependency}:{dependency_tag}`: {reason}")]
    IntegrityComputationFailed {
        dependency: String,
        dependency_tag: String,
        reason: String,
    },

    #[error(
        "failed to verify integrity for {interface_kind} `{interface_name}` from `{dependency}:{dependency_tag}`: interface not found"
    )]
    InterfaceNotFound {
        dependency: String,
        dependency_tag: String,
        interface_kind: InterfaceKind,
        interface_name: String,
    },

    #[error(
        "integrity mismatch for {interface_kind} `{interface_name}` from `{dependency}:{dependency_tag}` (expected `{expected}`, node stack has `{actual}`)"
    )]
    IntegrityMismatch {
        dependency: String,
        dependency_tag: String,
        interface_kind: InterfaceKind,
        interface_name: String,
        expected: String,
        actual: String,
    },
}

#[derive(Debug)]
pub struct DependencyInterfaceIntegrityErrors {
    pub errors: Vec<DependencyInterfaceIntegrityError>,
}

impl DependencyInterfaceIntegrityErrors {
    pub fn new(errors: Vec<DependencyInterfaceIntegrityError>) -> Self {
        Self { errors }
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl core::fmt::Display for DependencyInterfaceIntegrityErrors {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.errors.len() == 1 {
            return write!(f, "{}", self.errors[0]);
        }

        writeln!(
            f,
            "{} dependency interface integrity errors:",
            self.errors.len()
        )?;
        for err in &self.errors {
            writeln!(f, "- {}", err)?;
        }
        Ok(())
    }
}

impl std::error::Error for DependencyInterfaceIntegrityErrors {}

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

    // -- dependency integrity
    #[error(transparent)]
    DependencyInterfaceIntegrity(#[from] DependencyInterfaceIntegrityErrors),

    // -- generator-internal
    #[error(transparent)]
    GeneratorError(#[from] generator::GeneratorError),

    // -- config parsing
    #[error(transparent)]
    ParsingError(#[from] config::ParsingError),

    // -- templates
    #[error("template rendering error: {0}")]
    Template(#[from] askama::Error),
}
