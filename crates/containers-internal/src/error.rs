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

    /// The host-runtime counterpart of [`Error::PathNotAccessibleInVm`].
    ///
    /// Kept separate because the generic message's advice is actively wrong
    /// here: it tells you to declare the path in `container.mount_paths`,
    /// which is exactly what a node binding `/dev/video0` already did, and to
    /// move it under `$HOME`, which you cannot do to a device node. The real
    /// constraint is that this daemon runs its containers in a VM that has no
    /// view of the Mac's devices, and no mount declaration can change that.
    #[error(
        "Bind mount source {path} lives under a host runtime tree (/dev, /proc, /run, \
         /sys). These resolve on the machine that actually runs the containers, and \
         this daemon runs them inside a Lima VM, so no container.mount_paths entry \
         will make this Mac's copy reachable. If it is a device node, that is the end \
         of it: a USB, serial or CAN adapter plugged into this Mac cannot be forwarded \
         into the guest — run the node on a Linux daemon, where it resolves directly. \
         If it is a runtime directory such as /run/user backing XDG_RUNTIME_DIR, the \
         node should not be binding the host's copy in the first place."
    )]
    HostRuntimePathNotInVm { path: String },

    #[error("Failed to sync apptainer installation to Lima VM: {0}")]
    LimaSyncFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    ConfigurationError(String),
}
