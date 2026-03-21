use super::facade::{Apptainer, Backend, is_uri};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[cfg(target_os = "linux")]
use super::facade::{check_fakeroot_deps, find_binary};
#[cfg(target_os = "linux")]
use crate::error::Error;

// ---------------------------------------------------------------------------
// Builder argument assembly tests
// ---------------------------------------------------------------------------

#[test]
fn test_run_command_builds_correct_args() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade.run("image.sif");
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(args[0], "run");
    assert!(
        args.last().unwrap().ends_with("image.sif"),
        "last arg should be the image path, got: {:?}",
        args
    );
}

#[test]
fn test_exec_command_builds_correct_args() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade.exec("container.sif", &["echo", "hello"]);
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(args[0], "exec");
    assert_eq!(args[args.len() - 2], "echo");
    assert_eq!(args[args.len() - 1], "hello");
}

#[test]
fn test_build_command_builds_correct_args() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let output = PathBuf::from(&home).join("test/output.sif");
    let def = PathBuf::from(&home).join("test/def.def");
    let cmd = facade.build(&output, &def);
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(args[0], "build");
    assert!(args[1].ends_with("test/output.sif"));
    assert!(args[2].ends_with("test/def.def"));
}

#[test]
fn test_bind_flag_accumulates() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let dev1 = format!("{home}/dev1");
    let dev2 = format!("{home}/dev2");

    let cmd = facade
        .run("image.sif")
        .bind(&dev1, None, None)
        .bind(&dev2, None, None);
    let args = cmd.build_args().expect("build_args should succeed");

    let bind_count = args.iter().filter(|a| *a == "--bind").count();
    assert_eq!(bind_count, 2, "should have 2 --bind flags, got: {:?}", args);
}

#[test]
fn test_bind_with_dest() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let src = format!("{home}/data");

    let cmd = facade.run("image.sif").bind(&src, Some("/mnt/data"), None);
    let args = cmd.build_args().expect("build_args should succeed");

    let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
    let bind_spec = &args[bind_idx + 1];
    assert!(
        bind_spec.ends_with("data:/mnt/data"),
        "bind spec should have src:dest format, got: {}",
        bind_spec
    );
}

#[test]
fn test_bind_with_opts() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let src = format!("{home}/data");

    let cmd = facade
        .run("image.sif")
        .bind(&src, Some("/mnt/data"), Some("ro"));
    let args = cmd.build_args().expect("build_args should succeed");

    let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
    let bind_spec = &args[bind_idx + 1];
    assert!(
        bind_spec.ends_with("data:/mnt/data:ro"),
        "bind spec should have src:dest:opts format, got: {}",
        bind_spec
    );
}

#[test]
fn test_binds_convenience() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let home = std::env::var("HOME").unwrap();
    let dev1 = format!("{home}/dev1");
    let dev2 = format!("{home}/dev2");
    let dev3 = format!("{home}/dev3");

    let cmd = facade.run("image.sif").binds(&[&dev1, &dev2, &dev3]);
    let args = cmd.build_args().expect("build_args should succeed");

    let bind_count = args.iter().filter(|a| *a == "--bind").count();
    assert_eq!(bind_count, 3, "should have 3 --bind flags, got: {:?}", args);
}

#[test]
fn test_env_flag_format() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade.run("image.sif").env("FOO", "bar");
    let args = cmd.build_args().expect("build_args should succeed");

    let env_idx = args.iter().position(|a| a == "--env").unwrap();
    assert_eq!(args[env_idx + 1], "FOO=bar");
}

#[test]
fn test_lima_shell_extra_args_does_not_affect_build_args() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade
        .run("image.sif")
        .lima_shell_extra_args(&["--timeout".to_string(), "30".to_string()]);
    let args = cmd.build_args().expect("build_args should succeed");

    // lima_shell_extra_args are passed to limactl, not to apptainer,
    // so they should NOT appear in build_args output.
    assert_eq!(args[0], "run");
    assert!(
        !args.contains(&"--timeout".to_string()),
        "lima_shell_extra_args should not appear in apptainer args: {:?}",
        args
    );
}

#[test]
fn test_raw_flag_passthrough() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade.run("image.sif").raw_flag("--force");
    let args = cmd.build_args().expect("build_args should succeed");

    assert!(
        args.contains(&"--force".to_string()),
        "should contain --force: {:?}",
        args
    );
}

#[test]
fn test_args_appended_after_image() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade.run("image.sif").args(&["--config", "app.yaml"]);
    let args = cmd.build_args().expect("build_args should succeed");

    // args should end with: [..., "image.sif", "--config", "app.yaml"]
    assert_eq!(args[args.len() - 2], "--config");
    assert_eq!(args[args.len() - 1], "app.yaml");
}

#[test]
fn test_flags_come_before_positional_args() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let cmd = facade
        .run("image.sif")
        .fakeroot()
        .writable_tmpfs()
        .contain();
    let args = cmd.build_args().expect("build_args should succeed");

    // Subcommand is first
    assert_eq!(args[0], "run");

    // Find the image position (it's the translated path ending in image.sif)
    let image_idx = args.iter().position(|a| a.ends_with("image.sif")).unwrap();

    // All flags should come before the image
    let fakeroot_idx = args.iter().position(|a| a == "--fakeroot").unwrap();
    let writable_idx = args.iter().position(|a| a == "--writable-tmpfs").unwrap();
    let contain_idx = args.iter().position(|a| a == "--contain").unwrap();

    assert!(
        fakeroot_idx < image_idx,
        "--fakeroot should come before image"
    );
    assert!(
        writable_idx < image_idx,
        "--writable-tmpfs should come before image"
    );
    assert!(
        contain_idx < image_idx,
        "--contain should come before image"
    );
}

// ---------------------------------------------------------------------------
// Construction tests
// ---------------------------------------------------------------------------

#[test]
fn test_from_valid_dir() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

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
fn test_from_nonexistent_dir() {
    let result = Apptainer::from_dir(PathBuf::from(
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
fn test_from_dir_fails_when_binary_missing() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let install_dir = tmp.path().join("apptainer");
    // Create the directory but NOT the bin/apptainer binary
    fs::create_dir_all(install_dir.join("bin")).unwrap();

    let result = Apptainer::from_dir(install_dir);
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
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

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
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

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
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

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

/// macOS `tempfile::tempdir()` creates directories under `/var/folders/...`,
/// which is NOT mounted in the Lima VM. This test documents that
/// `translate_path()` correctly rejects such paths on macOS.
#[test]
fn test_translate_path_rejects_var_folders() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let path = Path::new("/var/folders/T4/random123abc/T/tempdir/output.sif");
    let result = facade.translate_path(path);

    if cfg!(target_os = "macos") {
        assert!(
            result.is_err(),
            "Paths under /var/folders should be rejected under Lima (not mounted in guest)"
        );
    } else {
        assert_eq!(
            result.unwrap(),
            path,
            "On Linux, all absolute paths should pass through unchanged"
        );
    }
}

/// Verifies that `translate_path()` accepts paths outside `$HOME` when they have
/// been registered in `extra_mounts` (simulating what `ensure_host_mounts()` does).
#[test]
fn test_translate_path_accepts_registered_extra_mount() {
    let mut facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let mount_dir = PathBuf::from("/var/folders/T4/random123abc/T/tempdir");
    let file_in_mount = mount_dir.join("output.sif");

    if cfg!(target_os = "macos") {
        // Before registration: should be rejected
        assert!(
            facade.translate_path(&file_in_mount).is_err(),
            "Path outside $HOME should be rejected before registration"
        );

        // Register the mount directory
        facade.extra_mounts.push(mount_dir);

        // After registration: should be accepted
        let result = facade.translate_path(&file_in_mount);
        assert!(
            result.is_ok(),
            "Path under a registered extra mount should be accepted, got: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), file_in_mount);
    } else {
        // On Linux, all paths pass through regardless
        assert!(facade.translate_path(&file_in_mount).is_ok());
    }
}

/// Verifies that `build().build_args()` rejects paths outside `$HOME` on macOS,
/// exercising the full command-builder pipeline (not just `translate_path` directly).
#[test]
fn test_build_args_rejects_path_outside_home() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    let output = Path::new("/var/folders/xx/temp123/output.sif");
    let home = std::env::var("HOME").unwrap();
    let def = PathBuf::from(&home).join("project/test.def");

    let cmd = facade.build(output, &def);
    let result = cmd.build_args();

    if cfg!(target_os = "macos") {
        assert!(
            result.is_err(),
            "build_args() should reject output paths outside $HOME under Lima"
        );
    } else {
        assert!(result.is_ok(), "On Linux, all paths should be accepted");
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
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

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

/// Integration test: after `Apptainer::new()`, the Lima instance should
/// be running.
///
/// On macOS, this verifies that the peppy instance was created with the correct
/// template and is in "Running" state. On Linux, Lima is not used, so we assert
/// the backend is Native.
#[test]
fn test_lima_instance_running_after_init() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    match &facade.backend {
        Backend::Lima {
            limactl_path,
            lima_home,
            ..
        } => {
            let output = Command::new(limactl_path)
                .env("LIMA_HOME", lima_home)
                .args(["list", "--format", "{{.Status}}", "peppy"])
                .stdin(Stdio::null())
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

// ---------------------------------------------------------------------------
// Host gateway tests
// ---------------------------------------------------------------------------

#[test]
fn test_host_gateway_returns_correct_value() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    if cfg!(target_os = "macos") {
        assert_eq!(
            facade.host_gateway(),
            Some("host.lima.internal"),
            "On macOS (Lima), host_gateway() should return the Lima host gateway hostname"
        );
    } else {
        assert_eq!(
            facade.host_gateway(),
            None,
            "On Linux (Native), host_gateway() should return None"
        );
    }
}

// ---------------------------------------------------------------------------
// Fakeroot dependency pre-flight check tests (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn test_find_binary_returns_path_when_present() {
    let dir = TempDir::new().expect("create temp dir");
    let binary = dir.path().join("my_binary");
    fs::write(&binary, "#!/bin/sh\n").expect("write fake binary");

    let result = find_binary("my_binary", &[dir.path().to_path_buf()]);
    assert_eq!(result, Some(binary));
}

#[cfg(target_os = "linux")]
#[test]
fn test_find_binary_returns_none_when_absent() {
    let dir = TempDir::new().expect("create temp dir");

    let result = find_binary("nonexistent_binary", &[dir.path().to_path_buf()]);
    assert_eq!(result, None);
}

#[cfg(target_os = "linux")]
#[test]
fn test_check_fakeroot_deps_succeeds_when_both_present_and_setuid() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("create temp dir");

    for name in &["newuidmap", "newgidmap"] {
        let path = dir.path().join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write fake binary");
        let mut perms = fs::metadata(&path).expect("read metadata").permissions();
        perms.set_mode(0o4755);
        fs::set_permissions(&path, perms).expect("set permissions");

        // The kernel may silently strip setuid on nosetuid-mounted tmpfs.
        let actual_mode = fs::metadata(&path)
            .expect("read metadata")
            .permissions()
            .mode();
        if actual_mode & 0o4000 == 0 {
            eprintln!(
                "SKIPPING: filesystem stripped setuid bit (mode: {actual_mode:#o}). \
                 This is expected on nosetuid-mounted tmpfs."
            );
            return;
        }
    }

    let result = check_fakeroot_deps(&[dir.path().to_path_buf()]);
    assert!(
        result.is_ok(),
        "should succeed when both binaries are present with setuid, got: {:?}",
        result.unwrap_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn test_check_fakeroot_deps_fails_when_newuidmap_missing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("create temp dir");

    // Only create newgidmap, not newuidmap
    let path = dir.path().join("newgidmap");
    fs::write(&path, "#!/bin/sh\n").expect("write fake binary");
    let mut perms = fs::metadata(&path).expect("read metadata").permissions();
    perms.set_mode(0o4755);
    fs::set_permissions(&path, perms).expect("set permissions");

    let err = check_fakeroot_deps(&[dir.path().to_path_buf()])
        .expect_err("should fail when newuidmap is missing");

    match err {
        Error::FakerootDepsNotFound { binary, details } => {
            assert_eq!(binary, "newuidmap");
            assert!(
                details.contains("not found"),
                "expected 'not found' in details, got: {details}"
            );
        }
        other => panic!("expected FakerootDepsNotFound, got: {other:?}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_check_fakeroot_deps_fails_when_newgidmap_missing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("create temp dir");

    // Only create newuidmap, not newgidmap
    let path = dir.path().join("newuidmap");
    fs::write(&path, "#!/bin/sh\n").expect("write fake binary");
    let mut perms = fs::metadata(&path).expect("read metadata").permissions();
    perms.set_mode(0o4755);
    fs::set_permissions(&path, perms).expect("set permissions");

    // If the kernel stripped setuid, this test would fail for the wrong reason
    // (newuidmap found but without setuid). Skip in that case.
    let actual_mode = fs::metadata(&path)
        .expect("read metadata")
        .permissions()
        .mode();
    if actual_mode & 0o4000 == 0 {
        eprintln!("SKIPPING: filesystem stripped setuid bit");
        return;
    }

    let err = check_fakeroot_deps(&[dir.path().to_path_buf()])
        .expect_err("should fail when newgidmap is missing");

    match err {
        Error::FakerootDepsNotFound { binary, details } => {
            assert_eq!(binary, "newgidmap");
            assert!(
                details.contains("not found"),
                "expected 'not found' in details, got: {details}"
            );
        }
        other => panic!("expected FakerootDepsNotFound, got: {other:?}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_check_fakeroot_deps_fails_when_missing_setuid_bit() {
    let dir = TempDir::new().expect("create temp dir");

    for name in &["newuidmap", "newgidmap"] {
        let path = dir.path().join(name);
        fs::write(&path, "#!/bin/sh\n").expect("write fake binary");
        // Mode 0755 — no setuid bit
    }

    let err = check_fakeroot_deps(&[dir.path().to_path_buf()])
        .expect_err("should fail when setuid bit is missing");

    match err {
        Error::FakerootDepsNotFound { details, .. } => {
            assert!(
                details.contains("setuid"),
                "expected 'setuid' in details, got: {details}"
            );
        }
        other => panic!("expected FakerootDepsNotFound, got: {other:?}"),
    }
}
