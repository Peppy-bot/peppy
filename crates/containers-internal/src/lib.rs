mod apptainer;
mod error;

pub use apptainer::Apptainer;
#[cfg(target_os = "linux")]
pub use apptainer::{SetupStatus, check_setup_status};
pub use error::{Error, Result};

/// Pinned Apptainer version bundled at build time.
pub const APPTAINER_VERSION: &str = env!("APPTAINER_VERSION");
/// Pinned Lima version bundled at build time.
pub const LIMA_VERSION: &str = env!("LIMA_VERSION");
