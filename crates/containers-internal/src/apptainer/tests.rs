use super::facade::{ApptainerFacade, is_uri, parse_lima_version};
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
///
/// Sets `limactl_path` and `lima_home` to `None` (tests that need Lima routing
/// should use the integration test path instead).
fn mock_facade(install_dir: PathBuf, use_lima: bool) -> ApptainerFacade {
    let bin = install_dir.join("bin/apptainer");
    let guest_bin = if use_lima {
        PathBuf::from("/tmp/peppy/apptainer/bin/apptainer")
    } else {
        bin.clone()
    };
    ApptainerFacade {
        apptainer_dir: install_dir,
        apptainer_bin: bin,
        guest_apptainer_bin: guest_bin,
        use_lima,
        limactl_path: None,
        lima_home: None,
    }
}

#[test]
fn test_facade_from_valid_dir() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = create_mock_install_dir(&tmp);

    // Use mock_facade instead of from_dir to avoid requiring a real Lima setup.
    // The full from_dir + Lima integration path is covered by test_apptainer_version_integration.
    let facade = mock_facade(install_dir.clone(), cfg!(target_os = "macos"));
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
    let facade = mock_facade(install_dir.clone(), false);

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
// Path translation tests (use mock_facade to inject use_lima)
// ---------------------------------------------------------------------------

#[test]
fn test_translate_path_linux_passthrough() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, false);

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
fn test_effective_binary_path_linux() {
    let tmp = TempDir::new().unwrap();
    let install_dir = create_mock_install_dir(&tmp);
    let facade = mock_facade(install_dir, false);

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
    let facade = mock_facade(install_dir, true);

    assert_eq!(
        facade.effective_binary_path(),
        Path::new("/tmp/peppy/apptainer/bin/apptainer")
    );
    assert_ne!(
        facade.effective_binary_path(),
        facade.binary_path(),
        "Under Lima, effective_binary_path should differ from host binary_path"
    );
}

// ---------------------------------------------------------------------------
// Lima version parsing tests
// ---------------------------------------------------------------------------

#[test]
fn test_parse_lima_version_full_string() {
    assert_eq!(parse_lima_version("limactl version 1.1.0"), Some((1, 1, 0)));
}

#[test]
fn test_parse_lima_version_bare_version() {
    assert_eq!(parse_lima_version("1.0.2"), Some((1, 0, 2)));
}

#[test]
fn test_parse_lima_version_with_whitespace() {
    assert_eq!(
        parse_lima_version("  limactl version 0.19.1  \n"),
        Some((0, 19, 1))
    );
}

#[test]
fn test_parse_lima_version_invalid() {
    assert_eq!(parse_lima_version("not a version"), None);
    assert_eq!(parse_lima_version(""), None);
    assert_eq!(parse_lima_version("1.2"), None);
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

/// Integration test: after ApptainerFacade::new(), the Lima instance should be running.
///
/// On macOS, this verifies that the peppy instance was created with the correct template
/// and is in "Running" state. On Linux, Lima is not used, so we assert the fields are None.
#[test]
fn test_lima_instance_running_after_init() {
    let facade = ApptainerFacade::new()
        .expect("ApptainerFacade::new() should succeed — apptainer is bundled at compile time");

    if cfg!(target_os = "macos") {
        let limactl = facade
            .limactl_path
            .as_ref()
            .expect("limactl_path should be Some on macOS");
        let lima_home = facade
            .lima_home
            .as_ref()
            .expect("lima_home should be Some on macOS");

        let output = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["list", "--format", "{{.Status}}", "peppy"])
            .output()
            .expect("limactl list should execute successfully");

        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            status, "Running",
            "Lima peppy instance should be Running after ApptainerFacade::new(), got: '{}'",
            status
        );
    } else {
        assert!(
            facade.limactl_path.is_none(),
            "limactl_path should be None on Linux"
        );
        assert!(
            facade.lima_home.is_none(),
            "lima_home should be None on Linux"
        );
    }
}
