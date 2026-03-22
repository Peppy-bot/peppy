pub mod facade;
pub(crate) mod lima;

#[cfg(test)]
mod tests;

pub use facade::Apptainer;
#[cfg(target_os = "linux")]
pub use facade::{SetupStatus, check_setup_status};
