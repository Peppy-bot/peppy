use super::facade::ApptainerFacade;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Create a mock apptainer installation directory with the expected structure.
fn create_mock_install_dir(tmp: &TempDir) -> PathBuf {
    let install_dir = tmp.path().join("apptainer");
    let bin_dir = install_dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // Create a dummy apptainer binary (just needs to exist and be a file)
    let bin_path = bin_dir.join("apptainer");
    fs::write(&bin_path, "#!/bin/sh\necho mock-apptainer\n").unwrap();
    fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();

    install_dir
}

#[test]
fn test_facade_from_valid_dir() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);

    let facade = ApptainerFacade::from_dir(install_dir.clone());
    assert!(facade.is_ok(), "Error creating facade: {:?}", facade.err());

    let facade = facade.unwrap();
    assert_eq!(facade.install_dir(), install_dir);
    assert_eq!(facade.binary_path(), install_dir.join("bin/apptainer"));
}

#[test]
fn test_facade_from_nonexistent_dir() {
    let result = ApptainerFacade::from_dir(PathBuf::from(
        "/nonexistent/apptainer/dir/that/does/not/exist",
    ));
    assert!(result.is_err(), "Expected error for nonexistent directory");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("bin/apptainer not found"),
        "Expected 'bin/apptainer not found' in error, got: {}",
        err_msg
    );
}

#[test]
fn test_facade_from_dir_fails_when_binary_missing() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = tmp.path().join("apptainer");
    // Create the directory but NOT the bin/apptainer binary
    fs::create_dir_all(install_dir.join("bin")).unwrap();

    let result = ApptainerFacade::from_dir(install_dir);
    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("bin/apptainer not found"),
        "Expected 'bin/apptainer not found' in error, got: {}",
        err_msg
    );
}

#[test]
fn test_binary_path_matches_install_dir() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir.clone(),
        apptainer_bin: bin.clone(),
        guest_apptainer_bin: bin,
        use_lima: false,
    };

    assert_eq!(facade.binary_path(), install_dir.join("bin/apptainer"));
}

/// Integration test: resolve the real apptainer installation (from build.rs compile-time
/// path or system PATH) and run `apptainer --version`.
///
/// On macOS this exercises the Lima sync and routing path.
/// build.rs guarantees apptainer is bundled, so this test should always succeed.
#[test]
fn test_apptainer_version_integration() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    // On macOS, effective_binary_path should point to the guest-side installation.
    if cfg!(target_os = "macos") {
        let effective = facade.effective_binary_path();
        assert_eq!(
            effective,
            Path::new("/tmp/peppy/apptainer/bin/apptainer"),
            "On macOS, effective_binary_path should be the guest-side path, got: {}",
            effective.display()
        );
    }

    let version = facade.version();
    let v = version.expect("apptainer --version should succeed");
    assert!(
        v.contains("apptainer") || v.contains("1."),
        "Expected version string containing 'apptainer' or '1.', got: {}",
        v
    );
    eprintln!("apptainer version: {}", v);
}

// ---------------------------------------------------------------------------
// Path translation tests (use direct struct construction to inject use_lima)
// ---------------------------------------------------------------------------

#[test]
fn test_translate_path_linux_passthrough() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin.clone(),
        guest_apptainer_bin: bin,
        use_lima: false,
    };

    // Any path should pass through unchanged when use_lima = false.
    let path = Path::new("/some/random/path/outside/home");
    assert_eq!(facade.translate_path(path).unwrap(), path);

    let home_path = PathBuf::from(std::env::var("HOME").unwrap()).join("project/file.def");
    assert_eq!(facade.translate_path(&home_path).unwrap(), home_path);
}

#[test]
fn test_translate_path_lima_under_home() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let guest_bin = PathBuf::from("/tmp/peppy/apptainer/bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin,
        guest_apptainer_bin: guest_bin,
        use_lima: true,
    };

    let home = std::env::var("HOME").unwrap();
    let path = PathBuf::from(&home).join("projects/my_node/apptainer.def");
    assert_eq!(
        facade.translate_path(&path).unwrap(),
        path,
        "Paths under $HOME should pass through unchanged"
    );
}

#[test]
fn test_translate_path_lima_outside_home_errors() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let guest_bin = PathBuf::from("/tmp/peppy/apptainer/bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin,
        guest_apptainer_bin: guest_bin,
        use_lima: true,
    };

    let path = Path::new("/opt/external/file.def");
    let result = facade.translate_path(path);
    assert!(
        result.is_err(),
        "Paths outside $HOME should error under Lima"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not accessible inside the Lima VM"),
        "Error should mention Lima VM inaccessibility, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("/opt/external/file.def"),
        "Error should include the offending path, got: {}",
        err_msg
    );
}

#[test]
fn test_effective_binary_path_linux() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin.clone(),
        guest_apptainer_bin: bin,
        use_lima: false,
    };

    assert_eq!(
        facade.effective_binary_path(),
        facade.binary_path(),
        "On Linux, effective_binary_path should equal binary_path"
    );
}

#[test]
fn test_effective_binary_path_lima() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let bin = install_dir.join("bin/apptainer");
    let guest_bin = PathBuf::from("/tmp/peppy/apptainer/bin/apptainer");
    let facade = ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin,
        guest_apptainer_bin: guest_bin.clone(),
        use_lima: true,
    };

    assert_eq!(facade.effective_binary_path(), guest_bin);
    assert_ne!(
        facade.effective_binary_path(),
        facade.binary_path(),
        "Under Lima, effective_binary_path should differ from host binary_path"
    );
}
