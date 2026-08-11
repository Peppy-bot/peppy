pub mod facade;
pub(crate) mod lima;
pub mod usage;

#[cfg(test)]
mod tests;

pub use facade::Apptainer;
#[cfg(target_os = "linux")]
pub use facade::{SetupStatus, check_setup_status};
pub use usage::{CacheUsageProbe, effective_host_cache_dir};
