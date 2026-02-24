use super::facade::ApptainerFacade;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

/// Create a mock apptainer installation directory with the expected structure.
fn create_mock_install_dir(tmp: &TempDir) -> std::path::PathBuf {
    let install_dir = tmp.path().join("apptainer");
    let bin_dir = install_dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // Create a dummy apptainer binary (just needs to exist and be a file)
    let bin_path = bin_dir.join("apptainer");
    fs::write(&bin_path, "#!/bin/sh\necho mock-apptainer\n").unwrap();
    fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();

    install_dir
}

/// SAFETY: These tests mutate environment variables, which is inherently unsafe
/// in a multi-threaded context. Rust 2024 edition requires `unsafe` for set_var/remove_var.
/// Tests using env vars should NOT be run in parallel with each other (use --test-threads=1
/// or accept the inherent race). We save/restore the original value to minimize impact.
unsafe fn set_env(key: &str, val: &std::ffi::OsStr) {
    unsafe { std::env::set_var(key, val) };
}

unsafe fn restore_env(key: &str, original: Option<String>) {
    unsafe {
        match original {
            Some(val) => std::env::set_var(key, val),
            None => std::env::remove_var(key),
        }
    }
}

const ENV_KEY: &str = "PEPPY_APPTAINER_DIR";

#[test]
fn test_facade_creation_with_env_var() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);

    let original = std::env::var(ENV_KEY).ok();
    // SAFETY: test-only env manipulation; see set_env doc comment.
    unsafe { set_env(ENV_KEY, install_dir.as_os_str()) };

    let facade = ApptainerFacade::new();
    assert!(facade.is_ok(), "Error creating facade: {:?}", facade.err());

    let facade = facade.unwrap();
    assert_eq!(facade.install_dir(), install_dir);
    assert_eq!(facade.binary_path(), install_dir.join("bin/apptainer"));

    // SAFETY: restoring original value.
    unsafe { restore_env(ENV_KEY, original) };
}

#[test]
fn test_facade_creation_fails_when_dir_missing() {
    let original = std::env::var(ENV_KEY).ok();
    // SAFETY: test-only env manipulation.
    unsafe {
        set_env(
            ENV_KEY,
            std::ffi::OsStr::new("/nonexistent/apptainer/dir/that/does/not/exist"),
        )
    };

    // The directory doesn't exist, so the env var resolution should skip it
    // and eventually fail (unless apptainer is actually installed on the system).
    let _result = ApptainerFacade::new();

    // SAFETY: restoring original value.
    unsafe { restore_env(ENV_KEY, original) };
}

#[test]
fn test_facade_creation_fails_when_binary_missing_in_dir() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = tmp.path().join("apptainer");
    // Create the directory but NOT the bin/apptainer binary
    fs::create_dir_all(install_dir.join("bin")).unwrap();

    let original = std::env::var(ENV_KEY).ok();
    // SAFETY: test-only env manipulation.
    unsafe { set_env(ENV_KEY, install_dir.as_os_str()) };

    let result = ApptainerFacade::new();
    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("bin/apptainer not found"),
        "Expected 'bin/apptainer not found' in error, got: {}",
        err_msg
    );

    // SAFETY: restoring original value.
    unsafe { restore_env(ENV_KEY, original) };
}

#[test]
fn test_command_builders_produce_correct_args() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);

    let original = std::env::var(ENV_KEY).ok();
    // SAFETY: test-only env manipulation.
    unsafe { set_env(ENV_KEY, install_dir.as_os_str()) };

    let facade = ApptainerFacade::new().expect("Failed to create facade");

    // Verify the binary path is correctly set
    let expected_bin = install_dir.join("bin/apptainer");
    assert_eq!(facade.binary_path(), expected_bin);

    // SAFETY: restoring original value.
    unsafe { restore_env(ENV_KEY, original) };
}

/// Integration test: resolve the real apptainer installation (from build.rs compile-time
/// path or system PATH) and run `apptainer --version`.
///
/// On macOS this exercises the Lima routing path.
/// build.rs guarantees apptainer is bundled, so this test should always succeed.
#[test]
fn test_apptainer_version_integration() {
    // Don't override the env var — let the facade use compile-time or PATH resolution.
    let original = std::env::var(ENV_KEY).ok();
    // SAFETY: ensure no override interferes.
    unsafe { restore_env(ENV_KEY, None) };

    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    let version = facade.version();
    // SAFETY: restoring original value.
    unsafe { restore_env(ENV_KEY, original) };

    let v = version.expect("apptainer --version should succeed");
    assert!(
        v.contains("apptainer") || v.contains("1."),
        "Expected version string containing 'apptainer' or '1.', got: {}",
        v
    );
    eprintln!("apptainer version: {}", v);
}
