// The crate contains no `unsafe`: process control, IO, and path handling all go
// through safe std APIs. `forbid` (not `deny`) makes any future `unsafe` a hard
// compile error that cannot be locally silenced, so adding FFI (e.g. a host-native
// `libc::kill`) becomes a deliberate decision rather than a silent regression.
#![forbid(unsafe_code)]

mod apptainer;
mod error;
mod mount_source;

pub use apptainer::Apptainer;
pub use apptainer::{CacheUsageProbe, effective_host_cache_dir};
#[cfg(target_os = "linux")]
pub use apptainer::{SetupStatus, check_setup_status};
pub use error::{Error, Result};
pub use mount_source::{
    auto_created_warning, ensure_bind_source, is_host_provided_mount_source, mount_spec_source,
};

/// Pinned Apptainer version bundled at build time.
pub const APPTAINER_VERSION: &str = env!("APPTAINER_VERSION");
/// Pinned Lima version bundled at build time.
pub const LIMA_VERSION: &str = env!("LIMA_VERSION");
/// Pinned gocryptfs version shipped alongside the apptainer install.
///
/// Apptainer auto-discovers gocryptfs in `libexec/apptainer/bin/` and uses it
/// for encrypted overlay/image support, so bundling it lets that feature work
/// without requiring users to install gocryptfs via their distro package
/// manager.
pub const GOCRYPTFS_VERSION: &str = env!("GOCRYPTFS_VERSION");
/// Pinned squashfuse version compiled and shipped alongside the apptainer
/// install.
///
/// Apptainer auto-discovers `squashfuse_ll` in `libexec/apptainer/bin/` and
/// uses it to FUSE-mount a SIF's squashfs partition. Without it every
/// `apptainer run` first extracts the whole image into a temporary sandbox,
/// which is both slow and fatal on hosts whose `/tmp` is a quota-limited
/// tmpfs.
pub const SQUASHFUSE_VERSION: &str = env!("SQUASHFUSE_VERSION");
