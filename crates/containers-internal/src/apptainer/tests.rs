use super::facade::{Apptainer, Backend, is_uri};
#[cfg(target_os = "linux")]
use super::facade::{apparmor_profile_ref, check_setup_status, shell_escape_single_quoted};
use crate::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[cfg(unix)]
use super::facade::{GuestKillChild, await_guest_kill, wait_for_child_bounded};
#[cfg(unix)]
use std::process::ExitStatus;

// ---------------------------------------------------------------------------
// Shared test fixtures
// ---------------------------------------------------------------------------

/// Construct a fully-initialized `Apptainer` for integration tests that need
/// the real runtime, or `None` (after printing a SKIPPING diagnostic) when
/// this host does not meet the user namespace prerequisites. Prerequisites are
/// machine state (an AppArmor profile installed via `peppy container setup`),
/// not code under test, so integration tests self-skip on an unprovisioned
/// host, mirroring the setup-status tests below and the e2e suite in
/// tests/facade.rs. Pure command-assembly and path-translation tests use
/// [`native_facade`] / [`lima_facade`] instead: they must never depend on
/// host state.
fn ready_facade() -> Option<Apptainer> {
    if !host_meets_userns_prerequisites() {
        eprintln!(
            "SKIPPING: apptainer user namespace prerequisites not met on this host; \
             run `peppy container setup`"
        );
        return None;
    }
    Some(
        Apptainer::new()
            .expect("Apptainer::new() should succeed; apptainer is bundled at compile time"),
    )
}

/// Whether this host meets apptainer's user namespace prerequisites. Always
/// `true` off Linux: macOS routes through the Lima VM and has no AppArmor
/// prerequisites to check.
fn host_meets_userns_prerequisites() -> bool {
    #[cfg(target_os = "linux")]
    {
        check_setup_status(&ready_apptainer_dir()).is_ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Resolve the bundled apptainer install dir directly, bypassing the
/// construction-time readiness check in `new()`. Lets tests inspect the real
/// install layout even when prerequisites are not met, without booting the
/// Lima VM on macOS.
fn ready_apptainer_dir() -> PathBuf {
    Apptainer::resolve_apptainer_dir()
        .expect("resolve_apptainer_dir should succeed; apptainer is bundled at compile time")
}

/// Builds a Native-backend facade for command-assembly and path-translation
/// tests without touching host state. The apptainer path need not exist: these
/// tests only inspect assembled argv, translated paths, and the no-op kill path.
fn native_facade() -> Apptainer {
    Apptainer {
        apptainer_dir: PathBuf::from("/opt/apptainer"),
        backend: Backend::Native {
            apptainer_bin: PathBuf::from("/opt/apptainer/bin/apptainer"),
        },
        extra_mounts: Vec::new(),
    }
}

/// Builds a Lima-backend facade for command-assembly and path-translation
/// tests. The limactl/apptainer paths need not exist: nothing is spawned, only
/// argv construction and path translation are exercised.
fn lima_facade() -> Apptainer {
    Apptainer {
        apptainer_dir: PathBuf::from("/opt/apptainer"),
        backend: Backend::Lima {
            apptainer_bin: PathBuf::from("/tmp/peppy/apptainer/bin/apptainer"),
            limactl_path: PathBuf::from("/opt/lima/bin/limactl"),
            lima_home: PathBuf::from("/home/u/.lima"),
        },
        extra_mounts: Vec::new(),
    }
}

/// Whether this host's kernel restricts unprivileged user namespaces via
/// AppArmor (raw procfs flag, without the manageability gate production
/// applies on top). Used to skip the not-restricted setup-status test; the
/// restricted-state tests gate on `SetupStatus::apparmor_restricted` instead.
#[cfg(target_os = "linux")]
fn apparmor_restricts_userns() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Builder argument assembly tests
// ---------------------------------------------------------------------------

#[test]
fn test_run_command_builds_correct_args() {
    let facade = native_facade();

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
    let facade = native_facade();

    let cmd = facade.exec("container.sif", &["echo", "hello"]);
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(args[0], "exec");
    assert_eq!(args[args.len() - 2], "echo");
    assert_eq!(args[args.len() - 1], "hello");
}

#[test]
fn test_build_command_builds_correct_args() {
    let facade = native_facade();

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
    let facade = native_facade();

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
    let facade = native_facade();

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
    let facade = native_facade();

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
fn test_env_flag_format() {
    let facade = native_facade();

    let cmd = facade.run("image.sif").env("FOO", "bar");
    let args = cmd.build_args().expect("build_args should succeed");

    let env_idx = args.iter().position(|a| a == "--env").unwrap();
    assert_eq!(args[env_idx + 1], "FOO=bar");
}

#[test]
fn test_lima_shell_extra_args_does_not_affect_build_args() {
    let facade = lima_facade();
    let home = std::env::var("HOME").expect("HOME must be set");
    let sif = PathBuf::from(&home).join("peppy_extra_args_test/node.sif");

    let cmd = facade
        .run(sif.to_str().expect("utf-8 sif path"))
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

/// The complement of the test above: `lima_shell_extra_args` MUST reach the
/// assembled `limactl` argv, positioned before the `--` separator (so limactl,
/// not apptainer, consumes them). Driven through a Lima-backend facade so it is
/// fully deterministic and spawns nothing.
#[test]
fn test_lima_shell_extra_args_reach_limactl_argv_before_separator() {
    let facade = lima_facade();
    let home = std::env::var("HOME").expect("HOME must be set");
    let sif = PathBuf::from(&home).join("peppy_extra_args_test/node.sif");

    let cmd = facade
        .run(sif.to_str().expect("utf-8 sif path"))
        .lima_shell_extra_args(&["--timeout".to_string(), "30".to_string()])
        .into_std_command()
        .expect("Lima run command should assemble");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let separator = args
        .iter()
        .position(|a| a == "--")
        .expect("limactl argv must contain the -- separator");
    let timeout = args
        .iter()
        .position(|a| a == "--timeout")
        .expect("lima_shell_extra_args should reach the limactl argv");
    assert!(
        timeout < separator,
        "lima_shell_extra_args must precede the -- separator, got: {args:?}"
    );
    assert_eq!(
        args[timeout + 1],
        "30",
        "the extra-arg value should follow its flag"
    );
}

#[test]
fn test_raw_flag_passthrough() {
    let facade = native_facade();

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
    let facade = native_facade();

    let cmd = facade.run("image.sif").args(&["--config", "app.yaml"]);
    let args = cmd.build_args().expect("build_args should succeed");

    // args should end with: [..., "image.sif", "--config", "app.yaml"]
    assert_eq!(args[args.len() - 2], "--config");
    assert_eq!(args[args.len() - 1], "app.yaml");
}

#[test]
fn test_flags_come_before_positional_args() {
    let facade = native_facade();

    let cmd = facade
        .run("image.sif")
        .raw_flag("--writable-tmpfs")
        .raw_flag("--contain");
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
    let Some(facade) = ready_facade() else {
        return;
    };

    assert!(
        facade.apptainer_dir.is_dir(),
        "the resolved install dir should be a real directory, got: {}",
        facade.apptainer_dir.display()
    );
    let apptainer_bin = match &facade.backend {
        Backend::Native { apptainer_bin } | Backend::Lima { apptainer_bin, .. } => apptainer_bin,
    };
    assert!(
        !apptainer_bin.as_os_str().is_empty(),
        "the apptainer invocation binary path should be non-empty"
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
    let Some(facade) = ready_facade() else {
        return;
    };

    // On macOS, the invocation binary should point to the guest-side installation.
    if cfg!(target_os = "macos") {
        let expected = PathBuf::from(env!("GUEST_APPTAINER_DIR")).join("bin/apptainer");
        match &facade.backend {
            Backend::Lima { apptainer_bin, .. } => assert_eq!(
                apptainer_bin,
                &expected,
                "On macOS, the invocation binary should be the guest-side path, got: {}",
                apptainer_bin.display()
            ),
            Backend::Native { .. } => unreachable!("macOS uses the Lima backend"),
        }
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
    let home = std::env::var("HOME").unwrap();
    let path = PathBuf::from(&home).join("projects/my_node/apptainer.def");

    for facade in [native_facade(), lima_facade()] {
        assert_eq!(
            facade.translate_path(&path).unwrap(),
            path,
            "Paths under $HOME should pass through unchanged on both backends"
        );
    }
}

#[test]
fn test_translate_path_outside_home() {
    let path = Path::new("/opt/external/file.def");

    assert_eq!(
        native_facade().translate_path(path).unwrap(),
        path,
        "Native: paths outside $HOME should pass through unchanged"
    );

    let err_msg = lima_facade()
        .translate_path(path)
        .expect_err("Lima: paths outside $HOME should be rejected")
        .to_string();
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

/// macOS `tempfile::tempdir()` creates directories under `/var/folders/...`,
/// which is NOT mounted in the Lima VM. `translate_path()` must reject such
/// paths on the Lima backend and pass them through on the native backend.
#[test]
fn test_translate_path_rejects_var_folders() {
    let path = Path::new("/var/folders/T4/random123abc/T/tempdir/output.sif");

    assert_eq!(
        native_facade().translate_path(path).unwrap(),
        path,
        "Native: all absolute paths should pass through unchanged"
    );
    assert!(
        lima_facade().translate_path(path).is_err(),
        "Lima: paths under /var/folders should be rejected (not mounted in guest)"
    );
}

/// Verifies that `translate_path()` accepts paths outside `$HOME` when they have
/// been registered in `extra_mounts` (simulating what `ensure_host_mounts()` does).
#[test]
fn test_translate_path_accepts_registered_extra_mount() {
    let mut facade = lima_facade();

    let mount_dir = PathBuf::from("/var/folders/T4/random123abc/T/tempdir");
    let file_in_mount = mount_dir.join("output.sif");

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
}

/// Verifies that `build().build_args()` rejects paths outside `$HOME` under
/// Lima, exercising the full command-builder pipeline (not just
/// `translate_path` directly), while the native backend accepts all paths.
#[test]
fn test_build_args_rejects_path_outside_home() {
    let output = Path::new("/var/folders/xx/temp123/output.sif");
    let home = std::env::var("HOME").unwrap();
    let def = PathBuf::from(&home).join("project/test.def");

    assert!(
        native_facade().build(output, &def).build_args().is_ok(),
        "Native: all paths should be accepted"
    );
    assert!(
        lima_facade().build(output, &def).build_args().is_err(),
        "Lima: build_args() should reject output paths outside $HOME"
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
fn test_translate_path_resolves_relative() {
    let facade = native_facade();

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
    let Some(facade) = ready_facade() else {
        return;
    };

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
            // On Linux, Lima is not used; this is expected.
        }
    }
}

// ---------------------------------------------------------------------------
// Host gateway tests
// ---------------------------------------------------------------------------

#[test]
fn test_host_gateway_returns_correct_value() {
    assert_eq!(
        native_facade().host_gateway(),
        None,
        "Native: apptainer shares the host network namespace, so no gateway"
    );
    assert_eq!(
        lima_facade().host_gateway(),
        Some("host.lima.internal"),
        "Lima: host_gateway() should return the Lima host gateway hostname"
    );
}

/// On non-macOS there is no VM, so `is_lima_ready()` is unconditionally `true`
/// and resolves no Lima state. (The macOS resolution-failure branches need a
/// real host environment and are covered by the integration path.)
#[cfg(not(target_os = "macos"))]
#[test]
fn is_lima_ready_is_true_on_native_backend() {
    assert!(Apptainer::is_lima_ready());
}

// ---------------------------------------------------------------------------
// check_setup_status tests (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_reports_real_installation() {
    // Use resolve_apptainer_dir() directly to avoid the ensure_ready() check
    // in Apptainer::new(), which would fail if setup isn't complete.
    let apptainer_dir = ready_apptainer_dir();

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
    let apparmor_restricted = apparmor_restricts_userns();

    if apparmor_restricted {
        eprintln!("SKIPPING: system restricts unprivileged user namespaces via AppArmor");
        return;
    }

    let apptainer_dir = ready_apptainer_dir();

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
    // not just that the file exists on disk. Gate on the status's own
    // apparmor_restricted (procfs flag AND manageability) rather than the raw
    // procfs flag: inside containers the flag can read "1" while AppArmor is
    // not manageable, and production treats that as not restricted.
    let apptainer_dir = ready_apptainer_dir();
    let status = check_setup_status(&apptainer_dir);

    if !status.apparmor_restricted {
        eprintln!("SKIPPING: system does not restrict unprivileged user namespaces via AppArmor");
        return;
    }

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

    // If both are true (and newuidmap is present), the full check passes.
    if status.apparmor_ok && status.apparmor_loaded && status.newuidmap_ok {
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
    // Gate on the status's own apparmor_restricted (procfs flag AND
    // manageability), matching production: inside containers the raw flag can
    // read "1" while no profile can exist, which would fail the asserts below.
    let apptainer_dir = ready_apptainer_dir();
    let status = check_setup_status(&apptainer_dir);

    if !status.apparmor_restricted {
        eprintln!("SKIPPING: system does not restrict unprivileged user namespaces via AppArmor");
        return;
    }

    // Read this install's profile and check if it references the current path.
    let profile = apparmor_profile_ref(&apptainer_dir);
    let profile_references_current_path = fs::read_to_string(&profile.file)
        .map(|content| content.contains(&profile.starter_path))
        .unwrap_or(false);

    if !profile_references_current_path {
        assert!(
            !status.apparmor_ok,
            "apparmor_ok should be false when profile references a stale path"
        );
        assert!(!status.is_ok(), "is_ok should be false with stale profile");
        let script = status.fix_script.as_ref().expect("fix_script should exist");
        assert!(
            script.contains(&format!("tee {}", profile.file.display())),
            "fix script should regenerate this install's profile, got: {script}"
        );
        assert!(
            script.contains(&shell_escape_single_quoted(&profile.starter_path)),
            "fix script should use the current starter path (shell-escaped), got: {script}"
        );
    }
}

/// Distinct installations must map to distinct AppArmor profiles: the
/// per-install naming is what keeps `peppy container setup` for one
/// installation (e.g. after an apptainer version bump renames the build
/// cache) from invalidating every other installation on the machine.
#[cfg(target_os = "linux")]
#[test]
fn apparmor_profile_is_namespaced_per_install_path() {
    let a = apparmor_profile_ref(Path::new("/opt/peppy-a/apptainer"));
    let b = apparmor_profile_ref(Path::new("/opt/peppy-b/apptainer"));

    assert_ne!(
        a.name, b.name,
        "distinct installs need distinct profile names"
    );
    assert_ne!(
        a.file, b.file,
        "distinct installs need distinct profile files"
    );

    let suffix = a
        .name
        .strip_prefix("peppy-apptainer-")
        .expect("profile name should carry the peppy-apptainer- prefix");
    assert_eq!(suffix.len(), 16, "hash suffix is a full 64-bit hex value");
    assert!(
        suffix.chars().all(|c| c.is_ascii_hexdigit()),
        "hash suffix must be hex, got: {suffix}"
    );
    assert!(
        a.file.starts_with("/etc/apparmor.d"),
        "profiles live in /etc/apparmor.d, got: {}",
        a.file.display()
    );

    // The name is persisted in /etc, so the derivation must be deterministic.
    let a_again = apparmor_profile_ref(Path::new("/opt/peppy-a/apptainer"));
    assert_eq!(a.name, a_again.name, "profile naming must be deterministic");
}

/// The starter path is interpolated inside the single-quoted `echo '...'`
/// body of the fix script; an embedded quote must not break out of it.
#[cfg(target_os = "linux")]
#[test]
fn shell_escape_single_quoted_survives_embedded_quotes() {
    assert_eq!(
        shell_escape_single_quoted("/home/o'brien/.peppy/starter"),
        r"/home/o'\''brien/.peppy/starter"
    );
    assert_eq!(shell_escape_single_quoted("/plain/path"), "/plain/path");
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
        "APPTAINER_INSTALL_DIR={} does not exist; was ~/.peppy deleted without rebuilding?",
        install_dir
    );

    let sentinel = path.join(format!(".peppy-version-{}", env!("APPTAINER_VERSION")));
    assert!(
        sentinel.exists(),
        "Cache sentinel {:?} is missing; the apptainer cache may be corrupt",
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
        "gocryptfs binary missing at {:?}; apptainer encryption support will be disabled",
        gocryptfs_bin
    );

    // Bundle the xray helper too; same archive, useful for inspecting
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
        "gocryptfs sentinel {:?} is missing; bundled binary may be stale",
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
    // The install dir is the *host-side* installation root for both backends:
    // on macOS the same layout (including libexec/) is synced into the Lima
    // guest, so the host-side location is what matters on every platform.
    // Resolved directly so this needs no host prerequisites and boots no VM.
    let apptainer_dir = ready_apptainer_dir();
    let expected = apptainer_dir.join("libexec/apptainer/bin/gocryptfs");

    assert!(
        expected.exists(),
        "gocryptfs should be bundled at {:?}",
        expected
    );
}

// ---------------------------------------------------------------------------
// Guest-side cancellation (Lima): the guest command (build or run) is wrapped as
// a process-group leader so `kill_guest_process_group` can SIGKILL the whole
// guest group on --force build cancel or on run-node teardown.
// ---------------------------------------------------------------------------

#[test]
fn lima_guest_pgid_argv_wraps_in_setsid_and_records_pgid() {
    let argv = super::lima::lima_guest_pgid_argv(
        Path::new("/opt/apptainer/bin/apptainer"),
        &["build", "/home/u/out.sif", "/home/u/node.def"],
        Path::new("/tmp/peppy/pgids/buildkey.pgid"),
    );

    // `setsid -w sh -c <fixed script> sh <pgid_file> <apptainer_bin> <args...>`:
    // the script is a constant and every value is passed as a positional param,
    // so nothing is interpolated or shell-escaped.
    assert_eq!(argv[0], "setsid");
    assert_eq!(argv[1], "-w");
    assert_eq!(argv[2], "sh");
    assert_eq!(argv[3], "-c");
    assert_eq!(
        argv[4],
        "d=$(dirname \"$1\"); mkdir -p \"$d\"; echo $$ > \"$1\"; \
         pgid=\"$1\"; shift; \"$@\"; __rc=$?; rm -f \"$pgid\"; exit $__rc",
        "the wrapper makes sh the group leader, records its PGID to the guest-native \
         pgid file ($1), runs apptainer as a child so its children inherit the group, \
         then removes the pgid file and forwards apptainer's exit status"
    );
    assert_eq!(
        argv[5], "sh",
        "the `$0` placeholder so the next value is `$1`"
    );
    assert_eq!(
        argv[6], "/tmp/peppy/pgids/buildkey.pgid",
        "`$1`: the pgid file"
    );
    assert_eq!(argv[7], "/opt/apptainer/bin/apptainer");
    assert_eq!(argv[8], "build");
    assert_eq!(argv[9], "/home/u/out.sif");
    assert_eq!(argv[10], "/home/u/node.def");
    assert_eq!(argv.len(), 11);
}

#[test]
fn lima_kill_pgid_argv_sigkills_the_whole_group() {
    let argv = super::lima::lima_kill_pgid_argv(Path::new("/tmp/peppy/pgids/buildkey.pgid"));
    // `sh -c <fixed script> sh <pgid_file>`: the pgid file is passed as `$1`, so
    // nothing is interpolated or shell-escaped. The negative PGID SIGKILLs the
    // whole group (apptainer + its %post children), then `rm -f` removes the pgid
    // file (the cancel path SIGKILLs the wrapper before it can self-clean).
    // Best-effort: a missing/already-dead group is not an error, so `cat`'s stderr
    // is silenced inside the substitution (the outer `2>/dev/null` covers `kill`
    // only, not the command substitution).
    assert_eq!(argv[0], "sh");
    assert_eq!(argv[1], "-c");
    assert_eq!(
        argv[2],
        "kill -KILL -\"$(cat \"$1\" 2>/dev/null)\" 2>/dev/null; rm -f \"$1\" 2>/dev/null; true"
    );
    assert_eq!(
        argv[3], "sh",
        "the `$0` placeholder so the next value is `$1`"
    );
    assert_eq!(
        argv[4], "/tmp/peppy/pgids/buildkey.pgid",
        "`$1`: the pgid file"
    );
    assert_eq!(argv.len(), 5);
}

#[test]
fn lima_terminate_pgid_argv_sigterms_without_removing_pgid_file() {
    let argv = super::lima::lima_terminate_pgid_argv(Path::new("/tmp/peppy/pgids/buildkey.pgid"));
    // Cooperative SIGTERM must leave the pgid file in place so the later
    // force-kill path can still target the same in-VM process group if needed.
    // It also must not SIGTERM the wrapper shell itself; signaling apptainer
    // lets apptainer forward shutdown into the container while the wrapper keeps
    // waiting and can remove the pgid file on a clean exit.
    assert_eq!(argv[0], "sh");
    assert_eq!(argv[1], "-c");
    assert_eq!(
        argv[2],
        "pgid=\"$(cat \"$1\" 2>/dev/null || true)\"; \
         if [ -n \"$pgid\" ]; then \
           children=\"$(cat \"/proc/$pgid/task/$pgid/children\" 2>/dev/null || true)\"; \
           for child in $children; do kill -TERM \"$child\" 2>/dev/null || true; done; \
         fi; \
         true"
    );
    assert_eq!(
        argv[3], "sh",
        "the `$0` placeholder so the next value is `$1`"
    );
    assert_eq!(
        argv[4], "/tmp/peppy/pgids/buildkey.pgid",
        "`$1`: the pgid file"
    );
    assert_eq!(argv.len(), 5);
}

/// The guest-build PGID path is guest-native (`/tmp/peppy/pgids/<key>.pgid`), so
/// it lives on the guest's tmpfs rather than the virtiofs host mount.
#[test]
fn guest_pgid_path_is_guest_native() {
    assert_eq!(
        super::lima::guest_pgid_path("foo"),
        PathBuf::from("/tmp/peppy/pgids/foo.pgid")
    );
}

/// Under Lima, a build is wrapped as a process-group leader that records its
/// PGID to the guest-native path and self-cleans (no `exec`, so `sh` survives to
/// remove the file). The guest path is passed through untranslated.
#[test]
fn lima_build_wraps_in_setsid_with_guest_native_pgid() {
    let facade = lima_facade();
    let home = std::env::var("HOME").expect("HOME must be set");
    // Output/def live under $HOME so Lima path translation accepts them.
    let out = PathBuf::from(&home).join("peppy_build_test/out.sif");
    let def = PathBuf::from(&home).join("peppy_build_test/node.def");

    let cmd = facade
        .build(&out, &def)
        .cancel_pgid("buildkey")
        .into_std_command()
        .expect("Lima build command should assemble");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(
        args.iter().any(|a| a == "setsid"),
        "Lima build must be wrapped in setsid, got: {args:?}"
    );
    // The pgid path is now its own argv element (a positional param to the
    // wrapper), not interpolated into the script string.
    assert!(
        args.iter().any(|a| a == "/tmp/peppy/pgids/buildkey.pgid"),
        "wrapper must pass the guest-native PGID path as an argv element, got: {args:?}"
    );
    let script = args
        .iter()
        .find(|a| a.contains("echo $$"))
        .expect("the wrapper records the PGID with `echo $$`");
    assert!(
        script.contains("mkdir -p"),
        "wrapper must create the guest-native PGID dir, got: {script}"
    );
    assert!(
        !script.contains("exec "),
        "wrapper must run apptainer as a child (no exec) so sh can self-clean, got: {script}"
    );
}

/// Under Lima, a run is wrapped as a process-group leader that records its PGID
/// to the guest-native path keyed by the instance id, mirroring the build path
/// so `kill_guest_process_group` can SIGKILL the in-VM workload on teardown. The
/// guest path is passed through untranslated.
#[test]
fn lima_run_wraps_in_setsid_with_guest_native_pgid() {
    let facade = lima_facade();
    let home = std::env::var("HOME").expect("HOME must be set");
    // The SIF lives under $HOME so Lima path translation accepts it.
    let sif = PathBuf::from(&home).join("peppy_run_test/node.sif");

    let cmd = facade
        .run(sif.to_str().expect("utf-8 sif path"))
        .cancel_pgid("inst-key")
        .into_std_command()
        .expect("Lima run command should assemble");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(
        args.iter().any(|a| a == "setsid"),
        "Lima run must be wrapped in setsid, got: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "/tmp/peppy/pgids/inst-key.pgid"),
        "wrapper must pass the instance-keyed guest-native PGID path, got: {args:?}"
    );
    // The wrapped argv still runs `apptainer run <sif>` as the child command.
    assert!(
        args.iter().any(|a| a == "run"),
        "wrapper must invoke `apptainer run`, got: {args:?}"
    );
}

/// Under Native (Linux) a run is a plain `apptainer run ...` with no pgid
/// wrapper: the host process-group SIGKILL reaches the container directly in the
/// shared namespace, so `cancel_pgid` is ignored.
#[test]
fn native_run_is_plain() {
    let facade = native_facade();

    let cmd = facade
        .run("/work/node.sif")
        .cancel_pgid("inst-key")
        .into_std_command()
        .expect("native run command should assemble");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(
        !args.iter().any(|a| a == "setsid"),
        "native run must not be wrapped, got: {args:?}"
    );
    assert_eq!(args[0], "run", "native run invokes apptainer directly");
    assert!(
        !args.iter().any(|a| a.contains(".pgid")),
        "native run must not reference a PGID file, got: {args:?}"
    );
}

/// Under Native (Linux) there is no VM: the command is a plain
/// `apptainer build ...` with no pgid wrapper, `kill_guest_process_group` is an Ok
/// no-op (the host process group already covers the whole tree in the shared
/// namespace), and `guest_command` runs directly on the host.
#[test]
fn native_build_is_plain_and_kill_is_a_noop() {
    let facade = native_facade();
    let out = PathBuf::from("/work/out.sif");
    let def = PathBuf::from("/work/node.def");

    let cmd = facade
        .build(&out, &def)
        .cancel_pgid("buildkey")
        .into_std_command()
        .expect("native build command should assemble");
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(
        !args.iter().any(|a| a == "setsid"),
        "native build must not be wrapped, got: {args:?}"
    );
    assert_eq!(args[0], "build", "native build runs apptainer directly");
    assert!(
        !args.iter().any(|a| a.contains(".pgid")),
        "native build must not reference a PGID file, got: {args:?}"
    );

    facade
        .kill_guest_process_group("buildkey")
        .expect("native kill_guest_process_group must be an Ok no-op");

    let output = facade
        .guest_command(&["echo", "peppy-native"])
        .expect("guest_command should run on the host under Native");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "peppy-native"
    );
}

/// `guest_command` with no arguments has nothing to run, so it must return a
/// configuration error rather than spawning an empty command. Deterministic:
/// native backend, no subprocess.
#[test]
fn guest_command_rejects_empty_args() {
    let facade = native_facade();
    let err = facade
        .guest_command(&[])
        .expect_err("guest_command with no args should error");
    match err {
        Error::ConfigurationError(msg) => {
            assert!(
                msg.contains("at least one argument"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("expected ConfigurationError, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Best-effort batch guest kill: platform/empty gating (deterministic, no VM)
// ---------------------------------------------------------------------------

/// An empty key slice returns immediately on every platform: nothing to kill,
/// no Lima resolution, no panic.
#[test]
fn kill_guest_process_groups_best_effort_is_noop_for_empty_keys() {
    Apptainer::kill_guest_process_groups_best_effort(&[]);
}

#[test]
fn terminate_guest_process_groups_best_effort_is_noop_for_empty_keys() {
    assert!(!Apptainer::terminate_guest_process_groups_best_effort(&[]));
}

/// On the native (Linux) backend the host process-group kill already reaped the
/// shared-namespace workload, so this returns without resolving or touching Lima
/// even for a non-empty key set.
#[cfg(not(target_os = "macos"))]
#[test]
fn kill_guest_process_groups_best_effort_is_noop_on_native() {
    Apptainer::kill_guest_process_groups_best_effort(&["some-instance-key".to_string()]);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn terminate_guest_process_groups_best_effort_is_noop_on_native() {
    assert!(!Apptainer::terminate_guest_process_groups_best_effort(&[
        "some-instance-key".to_string(),
    ]));
}

/// On the native (Linux) backend `ensure_host_mounts` is a pure no-op: all host
/// paths are already accessible, so it accepts any input and registers nothing.
#[cfg(not(target_os = "macos"))]
#[test]
fn ensure_host_mounts_is_noop_on_native() {
    let mut facade = native_facade();
    facade
        .ensure_host_mounts(&["/some/external/path"])
        .expect("native backend should accept any mounts as a no-op");
    assert!(
        facade.extra_mounts.is_empty(),
        "native backend registers no extra mounts"
    );
}

/// The bundled `LIMA_VERSION` pin must be present and shaped like a version
/// `parse_lima_version` can read, mirroring the bundled-binary checks that already
/// exist for APPTAINER_VERSION and GOCRYPTFS_VERSION.
#[test]
fn lima_version_const_is_present_and_parses() {
    assert!(
        !crate::LIMA_VERSION.is_empty(),
        "LIMA_VERSION should be set by build.rs"
    );
    assert!(
        super::lima::parse_lima_version(crate::LIMA_VERSION).is_some(),
        "LIMA_VERSION {:?} should parse as X.Y.Z",
        crate::LIMA_VERSION
    );
}

// ---------------------------------------------------------------------------
// await_guest_kill: bounded-wait decision logic, made deterministic via an
// injected clock and a fake child so the timeout/exit branches are covered with
// no real `limactl` subprocess and no wall-clock sleeping (the macOS integration
// test exercises only the happy path against a live VM).
// ---------------------------------------------------------------------------

/// A fake guest-kill child with a fixed exit state, so `await_guest_kill`'s
/// timeout/exit decision can be exercised without spawning `limactl`. Records
/// whether the timeout path killed it.
#[cfg(unix)]
struct FakeKillChild {
    /// `None` on every poll keeps the child "running" (drives the timeout path);
    /// `Some(status)` means it has already exited with that status.
    exit: Option<ExitStatus>,
    killed: bool,
}

#[cfg(unix)]
impl GuestKillChild for FakeKillChild {
    fn poll_exit(&mut self) -> crate::Result<Option<ExitStatus>> {
        Ok(self.exit)
    }

    fn kill_and_reap(&mut self) {
        self.killed = true;
    }
}

/// A clock that advances by `step` on each call, starting at `base`, so a test
/// can drive `await_guest_kill` deterministically past its deadline.
#[cfg(unix)]
fn stepping_clock(
    base: std::time::Instant,
    step: std::time::Duration,
) -> impl FnMut() -> std::time::Instant {
    let mut n: u32 = 0;
    move || {
        let now = base + step * n;
        n += 1;
        now
    }
}

#[cfg(unix)]
#[test]
fn await_guest_kill_returns_ok_when_child_exits_cleanly() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    let mut child = FakeKillChild {
        exit: Some(ExitStatus::from_raw(0)),
        killed: false,
    };
    let result = await_guest_kill(
        &mut child,
        Path::new("/tmp/peppy/pgids/k.pgid"),
        Duration::from_secs(10),
        Duration::from_millis(50),
        stepping_clock(Instant::now(), Duration::from_secs(1)),
        |_| panic!("must not sleep: the child has already exited"),
    );
    assert!(result.is_ok(), "a clean exit should be Ok, got: {result:?}");
    assert!(!child.killed, "a cleanly-exited child must not be killed");
}

#[cfg(unix)]
#[test]
fn await_guest_kill_reports_nonzero_exit() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    let mut child = FakeKillChild {
        exit: Some(ExitStatus::from_raw(1 << 8)),
        killed: false,
    };
    let err = await_guest_kill(
        &mut child,
        Path::new("/tmp/peppy/pgids/k.pgid"),
        Duration::from_secs(10),
        Duration::from_millis(50),
        stepping_clock(Instant::now(), Duration::from_secs(1)),
        |_| panic!("must not sleep: the child has already exited"),
    )
    .expect_err("a non-zero limactl exit should be an error");
    match err {
        Error::LimaInstanceError(msg) => {
            assert!(
                msg.contains("limactl exited with"),
                "unexpected message: {msg}"
            );
            assert!(
                msg.contains("/tmp/peppy/pgids/k.pgid"),
                "error should name the pgid file: {msg}"
            );
        }
        other => panic!("expected LimaInstanceError, got {other:?}"),
    }
    assert!(!child.killed, "an already-exited child must not be killed");
}

#[cfg(unix)]
#[test]
fn await_guest_kill_times_out_and_reaps_a_wedged_child() {
    use std::time::{Duration, Instant};

    // `exit: None` never reports an exit, so the deadline must fire. The clock
    // jumps a full timeout per call, so the first deadline check after the first
    // poll trips immediately (no real time passes).
    let mut child = FakeKillChild {
        exit: None,
        killed: false,
    };
    let err = await_guest_kill(
        &mut child,
        Path::new("/tmp/peppy/pgids/wedged.pgid"),
        Duration::from_secs(10),
        Duration::from_millis(50),
        stepping_clock(Instant::now(), Duration::from_secs(10)),
        |_| {},
    )
    .expect_err("a child that never exits should time out");
    match err {
        Error::LimaInstanceError(msg) => {
            assert!(msg.contains("timed out"), "unexpected message: {msg}");
            assert!(
                msg.contains("/tmp/peppy/pgids/wedged.pgid"),
                "error should name the pgid file: {msg}"
            );
        }
        other => panic!("expected LimaInstanceError, got {other:?}"),
    }
    assert!(
        child.killed,
        "the timeout path must kill and reap the wedged child"
    );
}

#[cfg(unix)]
#[test]
fn wait_for_child_bounded_returns_exit_status_on_clean_exit() {
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};

    let mut child = FakeKillChild {
        exit: Some(ExitStatus::from_raw(0)),
        killed: false,
    };
    let result = wait_for_child_bounded(
        &mut child,
        Duration::from_secs(10),
        Duration::from_millis(50),
        stepping_clock(Instant::now(), Duration::from_secs(1)),
        |_| panic!("must not sleep: the child has already exited"),
    );
    assert!(
        matches!(result, Ok(Some(status)) if status.success()),
        "a clean exit should yield Ok(Some(success)), got: {result:?}"
    );
    assert!(!child.killed, "a cleanly-exited child must not be killed");
}

#[cfg(unix)]
#[test]
fn wait_for_child_bounded_returns_none_and_reaps_on_timeout() {
    use std::time::{Duration, Instant};

    // `exit: None` never reports an exit, so the deadline must fire. The clock
    // jumps a full timeout per call, so the first deadline check after the first
    // poll trips immediately (no real time passes). This `Ok(None)` timeout
    // contract is what `is_ssh_alive` relies on to treat a wedged VM as
    // unreachable rather than panic or park its blocking thread.
    let mut child = FakeKillChild {
        exit: None,
        killed: false,
    };
    let result = wait_for_child_bounded(
        &mut child,
        Duration::from_secs(10),
        Duration::from_millis(50),
        stepping_clock(Instant::now(), Duration::from_secs(10)),
        |_| {},
    );
    assert!(
        matches!(result, Ok(None)),
        "a child that never exits should time out to Ok(None), got: {result:?}"
    );
    assert!(
        child.killed,
        "the timeout path must kill and reap the wedged child"
    );
}
