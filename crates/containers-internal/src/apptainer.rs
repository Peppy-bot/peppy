pub mod activity;
pub mod facade;
pub(crate) mod lima;
pub(crate) mod registry_auth;

#[cfg(test)]
mod tests;

pub use activity::{BuildActivity, BuildActivityProbe};
pub use facade::Apptainer;
#[cfg(target_os = "linux")]
pub use facade::{SetupStatus, check_setup_status};
