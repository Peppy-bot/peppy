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

    #[error("Failed to query Lima version: {0}")]
    LimaVersionCheckFailed(String),

    #[error("Lima instance management failed: {0}")]
    LimaInstanceError(String),

    #[error("Apptainer command `{command}` failed with {status}: {stderr}")]
    CommandFailed {
        command: String,
        status: ExitStatus,
        stderr: String,
    },

    #[error(
        "Path {path} is not accessible inside the Lima VM. Lima only mounts the home \
         directory (~) and explicitly configured mount paths into the guest. Ensure all \
         file paths are under your home directory, or configure them via \
         container.mount_paths in peppy.json5."
    )]
    PathNotAccessibleInVm { path: String },

    #[error("Failed to sync apptainer installation to Lima VM: {0}")]
    LimaSyncFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    ConfigurationError(String),

    #[error(
        "Fakeroot support requires `{binary}` with setuid-root permissions.\n\
         Install the `uidmap` package:\n\n  \
         sudo apt-get install uidmap    # Debian/Ubuntu\n  \
         sudo dnf install shadow-utils  # Fedora/RHEL\n\n\
         Details: {details}"
    )]
    FakerootDepsNotFound { binary: String, details: String },

    #[error(
        "AppArmor is blocking unprivileged user namespaces required by Apptainer.\n\
         Create a per-binary AppArmor profile to allow it:\n\n\
         {fix_command}\n"
    )]
    AppArmorUsernsRestricted { fix_command: String },
}
