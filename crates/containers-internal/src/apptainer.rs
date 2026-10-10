pub mod activity;
pub(crate) mod atomic_file;
pub mod facade;
pub(crate) mod l4t_manifest;
pub(crate) mod lima;
pub(crate) mod registry_auth;

#[cfg(test)]
mod tests;

/// A scratch directory under the shared test root, removed when dropped.
#[cfg(test)]
pub(crate) fn scratch_dir() -> tempfile::TempDir {
    tempfile::TempDir::new_in(config_test_support::test_tmp_root()).expect("create scratch dir")
}

pub use activity::{BuildActivity, BuildActivityProbe};
pub use facade::{Apptainer, ApptainerCommand};
#[cfg(target_os = "linux")]
pub use facade::{SetupStatus, check_setup_status};
