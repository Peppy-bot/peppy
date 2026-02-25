use super::facade::{ApptainerFacade, Backend, is_uri};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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

/// Create a mock `ApptainerFacade` for unit tests that don't need real Lima.
fn mock_facade(install_dir: PathBuf, lima: bool) -> ApptainerFacade {
    let backend = if lima {
        Backend::Lima {
            apptainer_bin: PathBuf::from("/tmp/peppy/apptainer/bin/apptainer"),
            limactl_path: PathBuf::from("/mock/limactl"),
            lima_home: PathBuf::from("/mock/lima-home"),
        }
    } else {
        Backend::Native {
            apptainer_bin: install_dir.join("bin/apptainer"),
        }
    };
    ApptainerFacade {
        apptainer_dir: install_dir,
        backend,
    }
}

// ---------------------------------------------------------------------------
// Construction tests
// ---------------------------------------------------------------------------

#[test]
fn test_facade_from_valid_dir() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);

    // Use mock_facade instead of from_dir to avoid requiring a real Lima setup.
    // The full from_dir + Lima integration path is covered by test_apptainer_version_integration.
    let facade = mock_facade(install_dir.clone(), cfg!(target_os = "macos"));
    assert_eq!(facade.install_dir(), install_dir);
    if cfg!(target_os = "macos") {
        assert_eq!(
            facade.binary_path(),
            Path::new("/tmp/peppy/apptainer/bin/apptainer"),
            "On macOS, binary_path should be the guest-side path"
        );
    } else {
        assert_eq!(facade.binary_path(), install_dir.join("bin/apptainer"));
    }
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

// ---------------------------------------------------------------------------
// Integration tests (require real apptainer/Lima)
// ---------------------------------------------------------------------------

/// Integration test: resolve the real apptainer installation (from build.rs compile-time
/// path or system PATH) and run `apptainer --version`.
///
/// On macOS this exercises the Lima sync and routing path.
/// build.rs guarantees apptainer is bundled, so this test should always succeed.
#[test]
fn test_apptainer_version_integration() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    // On macOS, binary_path should point to the guest-side installation.
    if cfg!(target_os = "macos") {
        let bin = facade.binary_path();
        assert_eq!(
            bin,
            Path::new("/tmp/peppy/apptainer/bin/apptainer"),
            "On macOS, binary_path should be the guest-side path, got: {}",
            bin.display()
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
// Path translation tests (use mock_facade to inject backend)
// ---------------------------------------------------------------------------

#[test]
fn test_translate_path_linux_passthrough() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, false);

    // Any path should pass through unchanged when using Native backend.
    let path = Path::new("/some/random/path/outside/home");
    assert_eq!(facade.translate_path(path).unwrap(), path);

    let home_path = PathBuf::from(std::env::var("HOME").unwrap()).join("project/file.def");
    assert_eq!(facade.translate_path(&home_path).unwrap(), home_path);
}

#[test]
fn test_translate_path_lima_under_home() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, true);

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
    let facade = mock_facade(install_dir, true);

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
fn test_binary_path_native() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir.clone(), false);

    assert_eq!(
        facade.binary_path(),
        install_dir.join("bin/apptainer"),
        "Native backend should use host-side binary path"
    );
}

#[test]
fn test_binary_path_lima() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, true);

    assert_eq!(
        facade.binary_path(),
        Path::new("/tmp/peppy/apptainer/bin/apptainer"),
        "Lima backend should use guest-side binary path"
    );
}

// ---------------------------------------------------------------------------
// URI detection tests
// ---------------------------------------------------------------------------

#[test]
fn test_is_uri() {
    // URI references should be detected
    assert!(is_uri("docker://ubuntu"));
    assert!(is_uri("library://default/ubuntu:latest"));
    assert!(is_uri("oras://registry.example.com/image:tag"));
    assert!(is_uri("shub://vsoch/hello-world"));

    // Filesystem paths should not be detected as URIs
    assert!(!is_uri("./my_image.sif"));
    assert!(!is_uri("/home/user/image.sif"));
    assert!(!is_uri("image.sif"));
    assert!(!is_uri("relative/path/to/image.sif"));
}

// ---------------------------------------------------------------------------
// Relative path translation tests
// ---------------------------------------------------------------------------

#[test]
fn test_translate_path_resolves_relative_linux() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, false);

    let relative = Path::new("my_image.sif");
    let result = facade.translate_path(relative).unwrap();

    assert!(
        result.is_absolute(),
        "Relative path should be resolved to absolute, got: {}",
        result.display()
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        result,
        cwd.join("my_image.sif"),
        "Relative path should resolve against CWD"
    );
}

#[test]
fn test_translate_path_resolves_relative_lima() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, true);

    // CWD is under $HOME during tests, so the resolved path should succeed.
    let relative = Path::new("project/my_image.sif");
    let result = facade.translate_path(relative).unwrap();

    assert!(
        result.is_absolute(),
        "Relative path should be resolved to absolute under Lima, got: {}",
        result.display()
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        result,
        cwd.join("project/my_image.sif"),
        "Relative path should resolve against CWD under Lima"
    );
}

// ---------------------------------------------------------------------------
// Lima instance status integration test
// ---------------------------------------------------------------------------

/// Integration test: after `ApptainerFacade::new()`, the Lima instance should
/// be running.
///
/// On macOS, this verifies that the peppy instance was created with the correct
/// template and is in "Running" state. On Linux, Lima is not used, so we assert
/// the backend is Native.
#[test]
fn test_lima_instance_running_after_init() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    match &facade.backend {
        Backend::Lima {
            limactl_path,
            lima_home,
            ..
        } => {
            let output = Command::new(limactl_path)
                .env("LIMA_HOME", lima_home)
                .args(["list", "--format", "{{.Status}}", "peppy"])
                .output()
                .expect("limactl list should execute successfully");

            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            assert_eq!(
                status, "Running",
                "Lima peppy instance should be Running after construction, got: '{}'",
                status
            );
        }
        Backend::Native { .. } => {
            // On Linux, Lima is not used — this is expected.
        }
    }
}
