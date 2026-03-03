mod apptainer;
mod error;

pub use apptainer::Apptainer;
pub use error::{Error, Result};

/// Pinned Apptainer version bundled at build time.
pub const APPTAINER_VERSION: &str = env!("APPTAINER_VERSION");
/// Pinned Lima version bundled at build time.
pub const LIMA_VERSION: &str = env!("LIMA_VERSION");
