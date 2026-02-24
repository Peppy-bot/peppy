use std::process::ExitStatus;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Apptainer installation not found: {0}")]
    ApptainerNotFound(String),

    #[error("Lima binary not found in peppy installation. Reinstall peppy or set PEPPY_LIMA_DIR.")]
    LimaRequired,

    #[error(
        "Lima version {found} is too old. peppy requires Lima >= {minimum}. \
         Reinstall peppy with an updated release."
    )]
    LimaVersionTooOld { found: String, minimum: String },

    #[error("Lima instance management failed: {0}")]
    LimaInstanceError(String),

    #[error("Apptainer command `{command}` failed with {status}: {stderr}")]
    CommandFailed {
        command: String,
        status: ExitStatus,
        stderr: String,
    },

    #[error(
        "Path {path} is not accessible inside the Lima VM. Lima auto-mounts the home directory (~). \
         Move your project files under your home directory, or set PEPPY_APPTAINER_DIR to a path under ~."
    )]
    PathNotAccessibleInVm { path: String },

    #[error("Failed to sync apptainer installation to Lima VM: {0}")]
    LimaSyncFailed(String),

    #[error("ApptainerFacade is not ready. Call ensure_ready() before running commands.")]
    NotReady,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    ConfigurationError(String),
}
