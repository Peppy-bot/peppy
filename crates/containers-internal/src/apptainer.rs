pub(crate) mod absent_bind_paths;
pub(crate) mod activity;
pub(crate) mod atomic_file;
pub(crate) mod command_scratch;
pub(crate) mod facade;
#[cfg(target_os = "linux")]
pub(crate) mod image_files;
pub(crate) mod l4t_manifest;
pub(crate) mod lima;
pub(crate) mod registry_auth;
#[cfg(target_os = "linux")]
pub(crate) mod root_user_namespace;
#[cfg(target_os = "linux")]
pub(crate) mod system_programs;
#[cfg(target_os = "linux")]
pub(crate) mod unprivileged_user_namespace;

#[cfg(test)]
mod tests;

/// A scratch directory under the shared test root, removed when dropped.
#[cfg(test)]
pub(crate) fn scratch_dir() -> tempfile::TempDir {
    tempfile::TempDir::new_in(config_test_support::test_tmp_root()).expect("create scratch dir")
}

pub use activity::{BuildActivity, BuildActivityProbe};
pub use command_scratch::remove_scratch_dir;
pub use facade::{Apptainer, ApptainerCommand, emulation_problem};
#[cfg(target_os = "linux")]
pub use facade::{SetupFix, SetupStatus, SetupVerdict, check_setup_status, check_system_programs};
#[cfg(target_os = "linux")]
pub use image_files::ImageFiles;
pub use registry_auth::RegistryAuthFile;
#[cfg(target_os = "linux")]
pub use system_programs::{
    FailureSeverity, ProgramCheck, ProgramState, SystemProgram, SystemPrograms,
};
#[cfg(target_os = "linux")]
pub use unprivileged_user_namespace::UnprivilegedUserNamespace;
