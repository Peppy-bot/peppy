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

    #[error(
        "{field} of the bind mount `{spec}` holds `{delimiter}`, which apptainer's \
         `src:dest:opts` spec reads as a separator and offers no escape for; mount the node \
         from a path without it"
    )]
    BindPathHoldsDelimiter {
        field: &'static str,
        spec: String,
        delimiter: char,
    },

    #[error(
        "the environment entry `{key}` cannot reach a node in the Lima VM: its name is empty or \
         holds `=` or a line break, or its value holds a line break"
    )]
    GuestEnvEntryUnsendable { key: String },

    #[error("Failed to prepare the apptainer scratch directory {path}: {source}")]
    ScratchDirUnavailable {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to read the registry auth file {path}: {source}")]
    RegistryAuthFileUnreadable {
        path: String,
        source: std::io::Error,
    },

    #[error(
        "The registry auth file {path} is not a valid Docker config file: {source}. \
         Apptainer reads registry credentials from it, so fix or remove it."
    )]
    RegistryAuthFileInvalid {
        path: String,
        source: serde_json::Error,
    },

    #[error("Failed to write the registry auth file handed to apptainer at {path}: {source}")]
    SanitizedRegistryAuthUnwritable {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to read the NVIDIA L4T container manifest {path}: {source}")]
    L4tManifestUnreadable {
        path: String,
        source: std::io::Error,
    },

    #[error("The NVIDIA L4T container manifest {path} is not valid at line {line}: {reason}")]
    L4tManifestInvalid {
        path: String,
        line: usize,
        reason: String,
    },

    #[error(
        "Failed to put the host's NVIDIA L4T driver libraries into the --nv library list \
         {path} of the bundled apptainer: {source}. A node that passes --nv on an NVIDIA \
         Jetson needs them, so the user that runs the daemon must be able to write this file."
    )]
    NvliblistUpdateFailed {
        path: String,
        source: std::io::Error,
    },

    /// A root daemon runs a container whose image its kernel cannot mount in
    /// a user namespace, and root cannot enter `closed_dir` from it, so the
    /// command cannot start its container. `compression` is the one of the
    /// squashfs of the image, `role` says what `path` is for the command, and
    /// `open_command` is the `chmod` command that lets root enter
    /// `closed_dir`.
    #[error(
        "This daemon runs as root and its kernel cannot mount the {compression} squashfs of \
         the image, so apptainer runs the container in a user namespace. In that namespace \
         root has its rights on the files of root only, and it cannot enter {closed_dir} on \
         the way to {role} {path}.\n\
         \n\
         To fix, do one of these:\n\
         \x20 - let root enter the directory: {open_command}\n\
         \x20 - keep peppy, its data and the mounts of a node in directories that root owns, \
         such as the home directory of root\n\
         \x20 - run the daemon as the user that owns the directory"
    )]
    ClosedToRootInUserNamespace {
        compression: &'static str,
        role: &'static str,
        path: String,
        closed_dir: String,
        open_command: String,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    ConfigurationError(String),

    /// This machine lacks a part of the emulation a build for another
    /// platform needs: the message names it and how to install it.
    #[error("{0}")]
    EmulationUnavailable(String),
}
