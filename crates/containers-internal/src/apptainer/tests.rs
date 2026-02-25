use super::facade::{ApptainerFacade, Backend, is_uri};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Construction tests
// ---------------------------------------------------------------------------

#[test]
fn test_facade_from_valid_dir() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    assert!(
        facade.install_dir().is_dir(),
        "install_dir() should be a real directory, got: {}",
        facade.install_dir().display()
    );
    assert!(
        !facade.binary_path().as_os_str().is_empty(),
        "binary_path() should be non-empty"
    );
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
        v.contains("apptainer"),
        "Expected version string containing 'apptainer', got: {}",
        v
    );
    eprintln!("apptainer version: {}", v);
}

// ---------------------------------------------------------------------------
// Path translation tests
// ---------------------------------------------------------------------------

#[test]
fn test_translate_path_under_home() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let path = PathBuf::from(&home).join("projects/my_node/apptainer.def");
    assert_eq!(
        facade.translate_path(&path).unwrap(),
        path,
        "Paths under $HOME should pass through unchanged"
    );
}

#[test]
fn test_translate_path_outside_home() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    let path = Path::new("/opt/external/file.def");
    let result = facade.translate_path(path);

    if cfg!(target_os = "macos") {
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
    } else {
        assert_eq!(
            result.unwrap(),
            path,
            "On Linux, paths outside $HOME should pass through unchanged"
        );
    }
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
fn test_translate_path_resolves_relative() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    let relative = Path::new("project/my_image.sif");
    let result = facade.translate_path(relative).unwrap();

    assert!(
        result.is_absolute(),
        "Relative path should be resolved to absolute, got: {}",
        result.display()
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        result,
        cwd.join("project/my_image.sif"),
        "Relative path should resolve against CWD"
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
