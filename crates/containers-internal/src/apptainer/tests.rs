#[cfg(target_os = "linux")]
use super::facade::check_setup_status;
use super::facade::{Apptainer, Backend, is_uri};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

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

    let cmd = facade.run("image.sif").writable_tmpfs().contain();
    let args = cmd.build_args().expect("build_args should succeed");

    // Subcommand is first
    assert_eq!(args[0], "run");

    // Find the image position (it's the translated path ending in image.sif)
    let image_idx = args.iter().position(|a| a.ends_with("image.sif")).unwrap();

    // All flags should come before the image
    let writable_idx = args.iter().position(|a| a == "--writable-tmpfs").unwrap();
    let contain_idx = args.iter().position(|a| a == "--contain").unwrap();

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
        let expected = PathBuf::from(env!("GUEST_APPTAINER_DIR")).join("bin/apptainer");
        assert_eq!(
            bin,
            expected,
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
// check_setup_status tests (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_reports_real_installation() {
    // Use resolve_apptainer_dir() directly to avoid the ensure_ready() check
    // in Apptainer::new(), which would fail if setup isn't complete.
    let apptainer_dir = Apptainer::resolve_apptainer_dir()
        .expect("resolve_apptainer_dir should succeed — apptainer is bundled at compile time");

    let status = check_setup_status(&apptainer_dir);

    // On systems with newuidmap and without AppArmor restrictions,
    // everything should pass.
    if status.newuidmap_ok && !status.apparmor_restricted {
        assert!(
            status.is_ok(),
            "is_ok should be true when newuidmap is available and no AppArmor restrictions"
        );
        assert!(
            status.fix_script.is_none(),
            "fix_script should be None when all checks pass"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_no_apparmor_restriction() {
    // On systems where AppArmor does not restrict user namespaces,
    // check_setup_status should report everything as OK.
    let apparmor_restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

    if apparmor_restricted {
        eprintln!("SKIPPING: system restricts unprivileged user namespaces via AppArmor");
        return;
    }

    let apptainer_dir = Apptainer::resolve_apptainer_dir()
        .expect("resolve_apptainer_dir should succeed — apptainer is bundled at compile time");

    let status = check_setup_status(&apptainer_dir);

    assert!(!status.apparmor_restricted);
    assert!(
        status.apparmor_ok,
        "apparmor_ok should be true when not restricted"
    );
    assert!(
        status.apparmor_loaded,
        "apparmor_loaded should be true when not restricted"
    );
    if status.newuidmap_ok {
        assert!(status.is_ok());
        assert!(status.fix_script.is_none());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_requires_apparmor_profile_loaded() {
    // On systems where AppArmor restricts unprivileged user namespaces,
    // check_setup_status must verify the profile is loaded into the kernel,
    // not just that the file exists on disk.
    let apparmor_restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

    if !apparmor_restricted {
        eprintln!("SKIPPING: system does not restrict unprivileged user namespaces via AppArmor");
        return;
    }

    let apptainer_dir = Apptainer::resolve_apptainer_dir()
        .expect("resolve_apptainer_dir should succeed — apptainer is bundled at compile time");

    let status = check_setup_status(&apptainer_dir);

    assert!(
        status.apparmor_restricted,
        "apparmor_restricted should be true on this system"
    );

    // If the profile file exists but isn't loaded, is_ok() must be false.
    if status.apparmor_ok && !status.apparmor_loaded {
        assert!(
            !status.is_ok(),
            "is_ok() should be false when profile is installed but not loaded"
        );
        let script = status.fix_script.as_ref().expect("fix_script should exist");
        assert!(
            script.contains("apparmor_parser"),
            "fix script should include apparmor_parser to load the profile, got: {script}"
        );
    }

    // If both are true, the full check should pass.
    if status.apparmor_ok && status.apparmor_loaded {
        assert!(
            status.is_ok(),
            "is_ok() should be true when all checks pass"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_detects_stale_apparmor_profile_path() {
    // When the AppArmor profile references a different starter path than
    // the current installation (e.g. a previous build artifact), apparmor_ok
    // must be false so the profile gets regenerated with the correct path.
    let apparmor_restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

    if !apparmor_restricted {
        eprintln!("SKIPPING: system does not restrict unprivileged user namespaces via AppArmor");
        return;
    }

    let apptainer_dir = Apptainer::resolve_apptainer_dir()
        .expect("resolve_apptainer_dir should succeed — apptainer is bundled at compile time");

    let status = check_setup_status(&apptainer_dir);

    // Read the installed profile and check if it references the current path.
    let starter = apptainer_dir.join("libexec/apptainer/bin/starter");
    let canonical = starter.canonicalize().unwrap_or_else(|_| starter.clone());

    let profile_references_current_path = fs::read_to_string("/etc/apparmor.d/peppy-apptainer")
        .map(|content| content.contains(&format!("{}", canonical.display())))
        .unwrap_or(false);

    if !profile_references_current_path {
        assert!(
            !status.apparmor_ok,
            "apparmor_ok should be false when profile references a stale path"
        );
        assert!(!status.is_ok(), "is_ok should be false with stale profile");
        let script = status.fix_script.as_ref().expect("fix_script should exist");
        assert!(
            script.contains("tee /etc/apparmor.d/peppy-apptainer"),
            "fix script should regenerate the profile, got: {script}"
        );
        assert!(
            script.contains(&format!("{}", canonical.display())),
            "fix script should use the current starter path, got: {script}"
        );
    }
}

// ---------------------------------------------------------------------------
// Compile-time cache consistency test
// ---------------------------------------------------------------------------

/// Verifies that the compile-time `APPTAINER_INSTALL_DIR` injected by build.rs
/// points to a valid cache directory with the expected sentinel and binary.
///
/// If this test fails after deleting `~/.peppy`, it means the build cache is
/// stale and `cargo build` needs to re-run build.rs (which the
/// `rerun-if-changed` directive on the sentinel file should ensure).
#[cfg(target_os = "linux")]
#[test]
fn compile_time_apptainer_dir_exists_with_sentinel() {
    let install_dir = env!(
        "APPTAINER_INSTALL_DIR",
        "APPTAINER_INSTALL_DIR should be set by build.rs at compile time"
    );

    let path = Path::new(install_dir);
    assert!(
        path.is_dir(),
        "APPTAINER_INSTALL_DIR={} does not exist — was ~/.peppy deleted without rebuilding?",
        install_dir
    );

    let sentinel = path.join(format!(".peppy-version-{}", env!("APPTAINER_VERSION")));
    assert!(
        sentinel.exists(),
        "Cache sentinel {:?} is missing — the apptainer cache may be corrupt",
        sentinel
    );

    let apptainer_bin = path.join("bin/apptainer");
    assert!(
        apptainer_bin.exists(),
        "bin/apptainer not found in APPTAINER_INSTALL_DIR={}",
        install_dir
    );
}

// ---------------------------------------------------------------------------
// gocryptfs bundling tests
//
// Apptainer searches `${prefix}/libexec/apptainer/bin/` for tools like
// gocryptfs ahead of `$PATH`. Bundling the binary there means encrypted
// overlays/images work without users having to install gocryptfs from their
// distro package manager.
// ---------------------------------------------------------------------------

/// Verifies the gocryptfs binary is bundled into the apptainer cache directory
/// where apptainer will discover it (`libexec/apptainer/bin/gocryptfs`).
#[cfg(target_os = "linux")]
#[test]
fn gocryptfs_bundled_in_apptainer_install_dir() {
    let install_dir = env!("APPTAINER_INSTALL_DIR");
    let path = Path::new(install_dir);

    let gocryptfs_bin = path.join("libexec/apptainer/bin/gocryptfs");
    assert!(
        gocryptfs_bin.exists(),
        "gocryptfs binary missing at {:?} — apptainer encryption support will be disabled",
        gocryptfs_bin
    );

    // Bundle the xray helper too — same archive, useful for inspecting
    // encrypted volumes.
    let gocryptfs_xray = path.join("libexec/apptainer/bin/gocryptfs-xray");
    assert!(
        gocryptfs_xray.exists(),
        "gocryptfs-xray helper missing at {:?}",
        gocryptfs_xray
    );

    // Sentinel encodes the version so a bump invalidates the cache.
    let sentinel = path.join("libexec/apptainer/bin").join(format!(
        ".peppy-gocryptfs-version-{}",
        crate::GOCRYPTFS_VERSION
    ));
    assert!(
        sentinel.exists(),
        "gocryptfs sentinel {:?} is missing — bundled binary may be stale",
        sentinel
    );
}

/// Runs the bundled `gocryptfs --version` and confirms it reports the pinned
/// release. This catches a corrupted/truncated extract that the existence
/// check above would miss.
#[cfg(target_os = "linux")]
#[test]
fn gocryptfs_bundled_binary_is_runnable() {
    let install_dir = env!("APPTAINER_INSTALL_DIR");
    let gocryptfs_bin = Path::new(install_dir).join("libexec/apptainer/bin/gocryptfs");

    let output = Command::new(&gocryptfs_bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "SKIPPING: cannot invoke bundled gocryptfs at {:?}: {} (likely a sandboxed test env)",
                    gocryptfs_bin, e
                );
                return;
            }
            _ => panic!(
                "unexpected error invoking bundled gocryptfs at {:?}: {} (kind: {:?})",
                gocryptfs_bin,
                e,
                e.kind()
            ),
        },
    };

    assert!(
        output.status.success(),
        "bundled gocryptfs --version should succeed (status: {})\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!("gocryptfs v{}", crate::GOCRYPTFS_VERSION);
    assert!(
        stdout.contains(&expected),
        "bundled gocryptfs version mismatch: expected to find {:?} in {:?}",
        expected,
        stdout
    );
}

/// Sanity check that the gocryptfs binary lives in apptainer's auto-discovery
/// path. Apptainer's `FindBin` looks here before `$PATH`, so its presence here
/// (combined with the runnability check above) means apptainer will pick it up
/// automatically with no environment manipulation.
#[test]
fn gocryptfs_path_matches_apptainer_search_dir() {
    let facade = Apptainer::new()
        .expect("Apptainer::new() should succeed — apptainer is bundled at compile time");

    // The install_dir is the *host-side* installation root for both backends.
    // For Lima, the same layout (including libexec/) is synced into the guest,
    // so the relative location is what matters.
    let expected = facade.install_dir().join("libexec/apptainer/bin/gocryptfs");

    if cfg!(target_os = "linux") {
        assert!(
            expected.exists(),
            "gocryptfs should be bundled at {:?}",
            expected
        );
    } else {
        // On macOS the host cache lives under ~/.peppy/tmp/... and is the
        // source of the guest sync; the same path must exist host-side.
        assert!(
            expected.exists(),
            "gocryptfs should be bundled host-side at {:?} for sync into Lima VM",
            expected
        );
    }
}
