use super::activity::{
    BuildActivity, BuildActivityProbe, ProcessCpu, build_cpu_time, parse_guest_activity,
    parse_ps_cputime, parse_ps_process_table,
};
use super::facade::{Apptainer, Backend, is_uri, prepare_scratch_dir};
#[cfg(target_os = "linux")]
use super::facade::{apparmor_profile_ref, check_setup_status, shell_escape_single_quoted};
use crate::error::Error;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
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

/// Scratch directory the [`native_facade`] fixture reports as its
/// `APPTAINER_TMPDIR`. A fixed literal, not the real resolved one: these tests
/// assert on assembled commands and must not depend on host state.
const NATIVE_FIXTURE_TMP_DIR: &str = "/opt/peppy-tmp/apptainer";

/// Builds a Native-backend facade for command-assembly and path-translation
/// tests without touching host state. The apptainer path need not exist: these
/// tests only inspect assembled argv, translated paths, and the no-op kill path.
fn native_facade() -> Apptainer {
    Apptainer {
        apptainer_dir: PathBuf::from("/opt/apptainer"),
        backend: Backend::Native {
            apptainer_bin: PathBuf::from("/opt/apptainer/bin/apptainer"),
            tmp_dir: PathBuf::from(NATIVE_FIXTURE_TMP_DIR),
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
fn test_apptainer_env_sets_process_env_on_native_build() {
    let facade = native_facade();

    let home = std::env::var("HOME").unwrap();
    let output = PathBuf::from(&home).join("test/output.sif");
    let def = PathBuf::from(&home).join("test/def.def");
    let cmd = facade
        .build(&output, &def)
        .apptainer_env("RUSTC_WRAPPER", "/peppy-cache/sccache");

    let args = cmd.build_args().expect("build_args should succeed");
    assert!(
        args.iter().all(|a| !a.contains("RUSTC_WRAPPER")),
        "apptainer_env must not appear in argv, got: {:?}",
        args
    );

    let std_cmd = cmd.into_std_command().expect("command should assemble");
    let env: Vec<_> = std_cmd.get_envs().collect();
    assert!(
        env.contains(&(
            std::ffi::OsStr::new("APPTAINERENV_RUSTC_WRAPPER"),
            Some(std::ffi::OsStr::new("/peppy-cache/sccache"))
        )),
        "expected APPTAINERENV_RUSTC_WRAPPER in process env, got: {:?}",
        env
    );
}

#[test]
fn test_apptainer_env_rides_the_guest_argv_on_lima_build() {
    let facade = lima_facade();

    let home = std::env::var("HOME").unwrap();
    let output = PathBuf::from(&home).join("test/output.sif");
    let def = PathBuf::from(&home).join("test/def.def");
    let std_cmd = facade
        .build(&output, &def)
        .apptainer_env("RUSTC_WRAPPER", "/peppy-cache/sccache")
        .into_std_command()
        .expect("command should assemble");

    let args: Vec<String> = std_cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let env_pos = args
        .iter()
        .position(|a| a == "env")
        .expect("guest argv must invoke env");
    assert_eq!(
        args[env_pos + 1],
        "APPTAINERENV_RUSTC_WRAPPER=/peppy-cache/sccache",
        "env must set the APPTAINERENV_ pair before execing apptainer, got: {:?}",
        args
    );
    assert!(
        std_cmd
            .get_envs()
            .all(|(key, _)| !key.to_string_lossy().starts_with("APPTAINERENV_")),
        "process env on limactl does not reach the guest and must stay unset"
    );
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

/// A value with a comma in it reaches the node whole: it rides the process
/// environment under the `APPTAINERENV_` prefix and never the `--env` flag,
/// whose argument apptainer splits at every comma.
#[test]
fn test_a_run_carries_a_comma_valued_variable_whole() {
    let facade = native_facade();

    let cmd = facade.run("image.sif").apptainer_env(
        "WORDS",
        "simulation=mujoco,alpha.robot_commander=mcp_commander",
    );
    let args = cmd.build_args().expect("build_args should succeed");
    assert!(
        !args.iter().any(|a| a == "--env"),
        "no variable rides the --env flag, got: {args:?}"
    );

    let std_cmd = cmd.into_std_command().expect("command should assemble");
    let env: Vec<_> = std_cmd.get_envs().collect();
    assert!(
        env.contains(&(
            std::ffi::OsStr::new("APPTAINERENV_WORDS"),
            Some(std::ffi::OsStr::new(
                "simulation=mujoco,alpha.robot_commander=mcp_commander"
            ))
        )),
        "expected APPTAINERENV_WORDS with the whole value in the process env, got: {env:?}"
    );
}

/// A path holding one of the spec's separators is refused rather than
/// spliced, since `src:dest:opts` offers no escape for them.
#[test]
fn test_a_bind_path_holding_a_separator_is_refused() {
    let facade = native_facade();

    for (src, dest) in [("/data:1", Some("/mnt")), ("/data", Some("/mnt,x"))] {
        let error = facade
            .run("image.sif")
            .bind(src, dest, None)
            .build_args()
            .expect_err("a separator in a bind path is refused");
        assert!(
            matches!(&error, crate::Error::BindPathHoldsDelimiter { delimiter, .. }
                if *delimiter == ':' || *delimiter == ','),
            "got: {error}"
        );
    }

    let args = facade
        .run("image.sif")
        .bind("/data", Some("/mnt"), Some("ro"))
        .build_args()
        .expect("an ordinary bind still builds");
    assert!(args.iter().any(|a| a == "/data:/mnt:ro"), "got: {args:?}");
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
        Backend::Native { apptainer_bin, .. } | Backend::Lima { apptainer_bin, .. } => {
            apptainer_bin
        }
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

/// A host runtime path stays rejected under Lima — passing it through to the
/// guest would only defer the failure to Apptainer — but it must be rejected
/// with the error that names the real constraint. The generic
/// `PathNotAccessibleInVm` advice (declare it in `container.mount_paths`, move
/// it under `$HOME`) is unfollowable for a device node, and telling an
/// operator to do the thing their node already does is worse than no advice.
#[test]
fn test_translate_path_rejects_host_runtime_paths_with_a_targeted_error() {
    for path in [
        "/dev/ttyUSB0",
        "/dev/can0",
        "/run/user/1000",
        "/proc/self",
        "/sys/class/net",
    ] {
        let path = Path::new(path);
        assert_eq!(
            native_facade().translate_path(path).unwrap(),
            path,
            "Native: a Linux daemon resolves these directly and must pass them through"
        );
        match lima_facade().translate_path(path) {
            Err(Error::HostRuntimePathNotInVm { path: reported }) => {
                assert_eq!(reported, path.display().to_string());
            }
            other => panic!(
                "Lima: {} should report HostRuntimePathNotInVm, got {other:?}",
                path.display()
            ),
        }
    }
}

/// A host runtime path must never reach the Lima mount list. The per-node run
/// path hands `ensure_host_mounts` the node's raw bind sources, so a node
/// declaring `mount_paths: ["/run/user"]` would otherwise have it written into
/// the Lima YAML — restarting the VM to mount the Mac's `/run/user` over the
/// guest's own — and then whitelisted in `extra_mounts`.
///
/// The facade's lima paths are fake, so reaching the YAML at all would error:
/// this passes only because the filter empties `external_paths` and takes the
/// early return.
#[test]
fn ensure_host_mounts_skips_host_provided_paths() {
    let mut facade = lima_facade();
    facade
        .ensure_host_mounts(&["/run/user", "/dev/ttyUSB0", "/proc/self", "/sys/class"])
        .expect("host-provided paths should be filtered out, not registered");
    assert!(
        facade.extra_mounts.is_empty(),
        "host runtime trees must never be registered as Lima mounts, got: {:?}",
        facade.extra_mounts
    );
}

/// Defence in depth for the check above: even if a host runtime path did end
/// up in `extra_mounts`, `translate_path` must still reject it. The guest's
/// `/run/user` is not the Mac's, whatever the mount list claims.
#[test]
fn test_translate_path_rejects_host_runtime_paths_despite_registration() {
    let mut facade = lima_facade();
    facade.extra_mounts.push(PathBuf::from("/run"));

    match facade.translate_path(Path::new("/run/user/1000")) {
        Err(Error::HostRuntimePathNotInVm { .. }) => {}
        other => panic!("a registered extra mount must not launder /run through, got {other:?}"),
    }
}

/// The complement: an ordinary unreachable path keeps the generic error, whose
/// advice does apply to it — put it under `$HOME`, or declare it as a mount.
#[test]
fn test_translate_path_keeps_generic_error_for_ordinary_paths() {
    let path = Path::new("/opt/robot_assets/openarm");
    match lima_facade().translate_path(path) {
        Err(Error::PathNotAccessibleInVm { path: reported }) => {
            assert_eq!(reported, path.display().to_string());
        }
        other => panic!("expected the generic mount-advice error, got {other:?}"),
    }
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

/// Verifies that the apptainer cache provisioned for this binary's
/// architecture exists with its completed-build sentinel and binary. The
/// location is derived at runtime from the exported cache names, so the
/// binary carries no path from the machine that built it.
///
/// If this test fails after deleting `~/.peppy`, it means the build cache is
/// stale and `cargo build` needs to re-run build.rs (which the
/// `rerun-if-changed` directive on the sentinel file should ensure).
#[cfg(target_os = "linux")]
#[test]
fn apptainer_cache_dir_exists_with_sentinel() {
    let Some(install_dir) = linux_apptainer_cache_dir() else {
        panic!("HOME is not set; cannot locate the apptainer build cache");
    };

    assert!(
        install_dir.is_dir(),
        "apptainer cache {:?} does not exist; was ~/.peppy deleted without rebuilding?",
        install_dir
    );

    let sentinel = install_dir.join(crate::APPTAINER_CACHE_SENTINEL_NAME);
    assert!(
        sentinel.exists(),
        "Cache sentinel {:?} is missing; the apptainer cache may be corrupt",
        sentinel
    );

    let apptainer_bin = install_dir.join("bin/apptainer");
    assert!(
        apptainer_bin.exists(),
        "bin/apptainer not found in the apptainer cache {:?}",
        install_dir
    );
}

/// The apptainer build cache for this binary's architecture, derived at
/// runtime exactly as production code derives it.
#[cfg(target_os = "linux")]
fn linux_apptainer_cache_dir() -> Option<PathBuf> {
    super::lima::peppy_home_dir(&format!("tmp/{}", crate::APPTAINER_CACHE_DIR_NAME))
}

/// The trees build.rs copies into OUT_DIR ship inside release archives: their
/// `.copy-source` sentinels and the generated apptainer man pages must not
/// carry the building machine's home directory.
#[test]
fn out_dir_trees_carry_no_build_machine_paths() {
    let Some(home) = std::env::var_os("HOME") else {
        panic!("HOME is not set; cannot check OUT_DIR trees for host paths");
    };
    let home = home.to_string_lossy();
    let out_dir = Path::new(env!("OUT_DIR"));

    for marker in [
        "apptainer-install/.copy-source",
        "lima-install/.copy-source",
    ] {
        let path = out_dir.join(marker);
        if let Ok(contents) = fs::read_to_string(&path) {
            assert!(
                !contents.contains(home.as_ref()),
                "{marker} embeds the building machine's home directory: {contents:?}"
            );
        }
    }

    let man_dir = out_dir.join("apptainer-install/share/man");
    if !man_dir.is_dir() {
        // A PEPPY_SKIP_APPTAINER_PROVISION build provisions nothing; there is
        // no tree to check, same self-skip the integration tests apply.
        eprintln!(
            "SKIPPING: no apptainer tree in OUT_DIR at {:?}; build without \
             PEPPY_SKIP_APPTAINER_PROVISION to check it",
            man_dir
        );
        return;
    }
    let mut stack = vec![man_dir];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read man page directory") {
            let entry = entry.expect("read man page entry");
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else if let Ok(contents) = fs::read_to_string(entry.path()) {
                assert!(
                    !contents.contains(home.as_ref()),
                    "{} embeds the building machine's home directory",
                    entry.path().display()
                );
            }
        }
    }
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
    let install_dir =
        linux_apptainer_cache_dir().expect("HOME is not set; cannot locate the apptainer cache");
    let path = install_dir;

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
    let install_dir =
        linux_apptainer_cache_dir().expect("HOME is not set; cannot locate the apptainer cache");
    let gocryptfs_bin = install_dir.join("libexec/apptainer/bin/gocryptfs");

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
// squashfuse bundling tests
//
// Apptainer mounts a SIF's squashfs partition through `squashfuse_ll`, found in
// `${prefix}/libexec/apptainer/bin/` ahead of `$PATH`. When it is missing the
// run still "works", just by extracting the whole rootfs into --tmpdir first,
// so nothing short of asserting on the binary itself catches its absence.
// ---------------------------------------------------------------------------

/// Verifies the squashfuse binary is bundled under the name apptainer's
/// `FindBin` looks for, in the directory it searches first.
#[test]
fn squashfuse_bundled_in_apptainer_install_dir() {
    // The install dir is the *host-side* installation root for both backends:
    // on macOS the same layout (including libexec/) is synced into the Lima
    // guest. Resolved directly so this needs no host prerequisites and boots
    // no VM.
    let squashfuse_bin = ready_apptainer_dir().join("libexec/apptainer/bin/squashfuse_ll");
    assert!(
        squashfuse_bin.exists(),
        "squashfuse_ll missing at {:?}; apptainer would extract every SIF into --tmpdir \
         instead of FUSE-mounting it",
        squashfuse_bin
    );
}

/// Runs the bundled `squashfuse_ll` and confirms it reports the pinned release.
/// This catches a truncated or wrong-architecture binary that the existence
/// check above would miss.
#[cfg(target_os = "linux")]
#[test]
fn squashfuse_bundled_binary_is_runnable() {
    let install_dir =
        linux_apptainer_cache_dir().expect("HOME is not set; cannot locate the apptainer cache");
    let squashfuse_bin = install_dir.join("libexec/apptainer/bin/squashfuse_ll");

    // Invoked with no arguments: `squashfuse_ll` has no `--version` flag, and
    // its usage banner carries the version. Apptainer scans the same output for
    // `-o uid=` to decide whether it can map ownership, so this doubles as a
    // check that the multithreaded build really does support it.
    let output = Command::new(&squashfuse_bin).stdin(Stdio::null()).output();

    let output = match output {
        Ok(o) => o,
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "SKIPPING: cannot invoke bundled squashfuse_ll at {:?}: {} (likely a sandboxed test env)",
                    squashfuse_bin, e
                );
                return;
            }
            _ => panic!(
                "unexpected error invoking bundled squashfuse_ll at {:?}: {} (kind: {:?})",
                squashfuse_bin,
                e,
                e.kind()
            ),
        },
    };

    // The usage banner goes to stderr on some builds and stdout on others;
    // apptainer merges the two streams for the same reason.
    let banner = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = format!("squashfuse {}", crate::SQUASHFUSE_VERSION);
    assert!(
        banner.contains(&expected),
        "bundled squashfuse version mismatch: expected to find {:?} in {:?}",
        expected,
        banner
    );
    assert!(
        banner.contains("-o uid="),
        "bundled squashfuse_ll does not advertise `-o uid=`, so apptainer will not map file \
         ownership into the container; banner was {:?}",
        banner
    );
}

// ---------------------------------------------------------------------------
// Scratch directory: apptainer is pointed away from the host's /tmp, which on
// some distros is a tmpfs under a systemd per-user quota.
// ---------------------------------------------------------------------------

/// Value an assembled command sets for `key`, or `None` when it sets none.
/// Inherited process environment is invisible here, which is what we want:
/// these tests assert on what the facade itself puts on the command.
fn command_env(cmd: &Command, key: &str) -> Option<PathBuf> {
    cmd.get_envs()
        .find(|(name, _)| name.to_str() == Some(key))
        .and_then(|(_, value)| value)
        .map(PathBuf::from)
}

/// The native backend exports its scratch directory as `APPTAINER_TMPDIR`, the
/// env spelling of apptainer's hidden `--tmpdir` flag.
#[test]
fn native_command_sets_apptainer_tmpdir() {
    let facade = native_facade();
    let cmd = facade
        .run("image.sif")
        .into_std_command()
        .expect("native command assembly should succeed");

    assert_eq!(
        command_env(&cmd, "APPTAINER_TMPDIR"),
        Some(PathBuf::from(NATIVE_FIXTURE_TMP_DIR)),
        "the native backend should point apptainer at its own scratch directory"
    );
}

/// The Lima backend leaves it unset: the command is handed to `limactl shell`,
/// which does not carry the host environment into the guest, so setting it
/// would only look like it worked.
#[test]
fn lima_command_does_not_set_apptainer_tmpdir() {
    let facade = lima_facade();
    // Under Lima the image path must be one the guest can see, i.e. under $HOME.
    let home = std::env::var("HOME").expect("HOME must be set");
    let sif = PathBuf::from(&home).join("peppy_tmpdir_test/node.sif");

    let cmd = facade
        .run(sif.to_str().expect("utf-8 sif path"))
        .into_std_command()
        .expect("lima command assembly should succeed");

    assert_eq!(
        command_env(&cmd, "APPTAINER_TMPDIR"),
        None,
        "a host-side APPTAINER_TMPDIR would not survive the hop into the guest"
    );
}

/// The happy path: a missing scratch directory is created, and the write probe
/// cleans up after itself rather than littering apptainer's `--tmpdir`.
#[test]
fn prepare_scratch_dir_creates_missing_directory() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let scratch = tmp.path().join("peppy-tmp/apptainer");

    prepare_scratch_dir(&scratch).expect("a fresh scratch directory should be usable");

    assert!(scratch.is_dir(), "the scratch directory should be created");
    assert_eq!(
        fs::read_dir(&scratch)
            .expect("scratch dir should be readable")
            .count(),
        0,
        "the write probe should leave nothing behind"
    );
}

/// Regression: concurrent preparation of the same scratch directory must
/// succeed on every thread. `Apptainer::new()` is called from several test
/// threads at once (and from more than one daemon thread in production), so a
/// write probe named only after the process id had them all racing on a single
/// path: the first `remove_file` to land won and the losers reported
/// `ScratchDirUnavailable { NotFound }` against a perfectly writable directory.
#[test]
fn prepare_scratch_dir_tolerates_concurrent_preparation() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let scratch = tmp.path().join("apptainer");

    let failures: Vec<Error> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|_| scope.spawn(|| prepare_scratch_dir(&scratch)))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("probe thread should not panic").err())
            .collect()
    });

    assert!(
        failures.is_empty(),
        "concurrent probes of a writable scratch directory should all succeed, got: {failures:?}"
    );
    assert_eq!(
        fs::read_dir(&scratch)
            .expect("scratch dir should be readable")
            .count(),
        0,
        "every write probe should clean up after itself"
    );
}

/// Regression: an existing but non-writable scratch directory must fail at
/// construction, not survive `create_dir_all` (which succeeds on any existing
/// directory, whatever its mode) only to fail later inside apptainer as an
/// opaque `MkdirTemp` error on the extract fallback.
#[cfg(unix)]
#[test]
fn prepare_scratch_dir_rejects_existing_non_writable_directory() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().expect("Failed to create temp dir");
    let scratch = tmp.path().join("apptainer");
    fs::create_dir(&scratch).expect("mkdir scratch");
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o500)).expect("chmod scratch");

    // Privileged processes ignore the mode bits, so confirm the directory is
    // genuinely unwritable here before asserting on the failure it should
    // produce; restore the mode either way so the TempDir can clean itself up.
    let bypasses_modes = fs::File::create(scratch.join(".privilege-check")).is_ok();
    let result = if bypasses_modes {
        Ok(())
    } else {
        prepare_scratch_dir(&scratch)
    };
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).expect("restore scratch mode");

    if bypasses_modes {
        eprintln!(
            "SKIPPING: this process writes into mode-0o500 directories, so an \
             unwritable scratch directory cannot be simulated"
        );
        return;
    }

    let err = result.expect_err("a non-writable scratch directory should be rejected");
    assert!(
        matches!(&err, Error::ScratchDirUnavailable { path, .. } if path == &scratch.display().to_string()),
        "expected ScratchDirUnavailable naming {:?}, got: {:?}",
        scratch,
        err
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
        None,
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
fn lima_guest_pgid_argv_with_workdir_cds_first_and_aborts_on_failure() {
    let argv = super::lima::lima_guest_pgid_argv(
        Path::new("/opt/apptainer/bin/apptainer"),
        &["build", "/var/folders/x/T/.peppy/tmp/b/out.sif", "node.def"],
        Path::new("/tmp/peppy/pgids/buildkey.pgid"),
        Some(Path::new("/var/folders/x/T/.peppy/tmp/b")),
    );

    // Same wrapper as above with the working dir prepended as `$1`: the script
    // `cd`s there FIRST and exits non-zero if it cannot. Relying on `limactl
    // shell`'s host-cwd propagation instead is what once copied the user's
    // entire $HOME into an image: the kernel canonicalizes the host cwd to
    // `/private/var/…`, the guest mount only exists at `/var/folders/…`, and
    // Lima's own failed `cd` is non-fatal, so `%files .` ran from the guest
    // home directory.
    assert_eq!(argv[0], "setsid");
    assert_eq!(argv[1], "-w");
    assert_eq!(argv[2], "sh");
    assert_eq!(argv[3], "-c");
    assert!(
        argv[4].starts_with(
            "cd \"$1\" || { echo \"peppy: guest working directory not accessible: $1\" >&2; \
             exit 1; }; shift; "
        ),
        "script must cd to the working dir first and abort loudly on failure, got: {}",
        argv[4]
    );
    assert!(
        argv[4].ends_with(
            "d=$(dirname \"$1\"); mkdir -p \"$d\"; echo $$ > \"$1\"; \
             pgid=\"$1\"; shift; \"$@\"; __rc=$?; rm -f \"$pgid\"; exit $__rc"
        ),
        "after the cd prelude the pgid wrapper must be unchanged, got: {}",
        argv[4]
    );
    assert_eq!(argv[5], "sh");
    assert_eq!(
        argv[6], "/var/folders/x/T/.peppy/tmp/b",
        "`$1`: the guest working dir, consumed by the prelude's shift"
    );
    assert_eq!(
        argv[7], "/tmp/peppy/pgids/buildkey.pgid",
        "`$1` after the shift: the pgid file"
    );
    assert_eq!(argv[8], "/opt/apptainer/bin/apptainer");
    assert_eq!(argv[9], "build");
    assert_eq!(argv.len(), 12);
}

#[test]
fn lima_guest_workdir_argv_cds_then_execs() {
    let argv = super::lima::lima_guest_workdir_argv(
        Path::new("/opt/apptainer/bin/apptainer"),
        &["run", "img.sif"],
        Path::new("/var/folders/x/T/.peppy/instances/i1"),
    );

    assert_eq!(argv[0], "sh");
    assert_eq!(argv[1], "-c");
    assert!(
        argv[2].starts_with("cd \"$1\" || "),
        "script must cd to the working dir first, got: {}",
        argv[2]
    );
    assert!(
        argv[2].ends_with("shift; exec \"$@\""),
        "script must exec apptainer after entering the working dir, got: {}",
        argv[2]
    );
    assert_eq!(argv[3], "sh");
    assert_eq!(argv[4], "/var/folders/x/T/.peppy/instances/i1");
    assert_eq!(argv[5], "/opt/apptainer/bin/apptainer");
    assert_eq!(argv[6], "run");
    assert_eq!(argv[7], "img.sif");
    assert_eq!(argv.len(), 8);
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

// ---------------------------------------------------------------------------
// Cache usage probe (build progress sampling)
// ---------------------------------------------------------------------------

#[test]
fn effective_host_cache_dir_prefers_the_env_override() {
    use super::activity::effective_host_cache_dir_from;
    use std::ffi::OsString;

    // The env override wins over $HOME, and an empty override is ignored the
    // way apptainer ignores it (fall back to the default location).
    let dir = effective_host_cache_dir_from(
        Some(OsString::from("/custom/cache")),
        Some(OsString::from("/home/user")),
    );
    assert_eq!(dir, Some(PathBuf::from("/custom/cache")));

    let dir =
        effective_host_cache_dir_from(Some(OsString::new()), Some(OsString::from("/home/user")));
    assert_eq!(dir, Some(PathBuf::from("/home/user/.apptainer/cache")));

    assert_eq!(effective_host_cache_dir_from(None, None), None);
}

#[test]
fn build_activity_probe_sums_a_tree_and_ignores_missing_roots() {
    let root = TempDir::new().expect("tempdir");
    fs::create_dir_all(root.path().join("blobs/sha256")).expect("mkdir");
    fs::write(root.path().join("blobs/sha256/aa"), vec![0u8; 1024]).expect("write");
    fs::write(root.path().join("blobs/sha256/bb"), vec![0u8; 512]).expect("write");
    fs::write(root.path().join("top"), vec![0u8; 64]).expect("write");

    let probe = BuildActivityProbe::host(
        vec![
            root.path().to_path_buf(),
            PathBuf::from("/nonexistent-peppy-usage-root"),
        ],
        None,
    );
    assert_eq!(probe.sample().bytes_on_disk, 1024 + 512 + 64);

    let missing_only =
        BuildActivityProbe::host(vec![PathBuf::from("/nonexistent-peppy-usage-root")], None);
    assert_eq!(missing_only.sample(), BuildActivity::default());
}

#[cfg(unix)]
#[test]
fn build_activity_probe_counts_a_symlink_itself_not_its_target() {
    // A symlink pointing outside the root must not pull the target's size (or
    // an unbounded tree) into the sample; only the link's own metadata counts.
    let target = TempDir::new().expect("tempdir");
    fs::write(target.path().join("big"), vec![0u8; 4096]).expect("write");
    let root = TempDir::new().expect("tempdir");
    fs::write(root.path().join("real"), vec![0u8; 128]).expect("write");
    std::os::unix::fs::symlink(target.path(), root.path().join("link")).expect("symlink");

    let probe = BuildActivityProbe::host(vec![root.path().to_path_buf()], None);
    let total = probe.sample().bytes_on_disk;
    assert!(
        (128..4096).contains(&total),
        "the symlink target's 4096-byte file must not be counted, got {total}"
    );
}

#[test]
fn parse_process_stat_counts_the_fields_from_the_last_parenthesis() {
    use super::activity::{ProcessCpu, parse_process_stat};

    // A command name holding spaces and parentheses of its own, as `/proc`
    // renders a process named "my (odd) worker".
    let line = "4242 (my (odd) worker) S 1 4200 4200 0 -1 4194304 110 0 0 0 7 3 2 1 \
                20 0 1 0 12345 6789 100 18446744073709551615";
    assert_eq!(
        parse_process_stat(line, TICKS_PER_SECOND),
        Some(ProcessCpu {
            pid: 4242,
            parent: 1,
            process_group: 4200,
            cpu_time: Duration::from_millis((7 + 3 + 2 + 1) * 10),
        })
    );
}

#[test]
fn parse_process_stat_converts_ticks_at_the_given_rate() {
    use super::activity::parse_process_stat;

    let line = "7 (worker) R 1 7 7 0 -1 0 0 0 0 0 200 50 0 0";
    let at = |hz| {
        parse_process_stat(line, NonZeroU64::new(hz).expect("nonzero"))
            .expect("the line parses")
            .cpu_time
    };
    assert_eq!(at(100), Duration::from_millis(2_500));
    assert_eq!(at(250), Duration::from_millis(1_000));
}

#[test]
fn parse_process_stat_rejects_a_line_missing_a_field() {
    use super::activity::parse_process_stat;

    assert_eq!(
        parse_process_stat(
            "4242 (short) S 1 4200 4200 0 -1 4194304 110 0 0 0 7 3 2",
            TICKS_PER_SECOND
        ),
        None,
        "a missing counter"
    );
    assert_eq!(
        parse_process_stat("no parenthesis at all", TICKS_PER_SECOND),
        None
    );
    // A command name holding a newline splits its line in two; neither half
    // reads as a process.
    assert_eq!(parse_process_stat("4242 (two", TICKS_PER_SECOND), None);
    assert_eq!(
        parse_process_stat(
            "lines) S 1 4200 4200 0 -1 4194304 110 0 0 0 7 3 2 1",
            TICKS_PER_SECOND
        ),
        None
    );
}

#[test]
fn parse_process_stat_counts_a_negative_reaped_counter_as_zero() {
    use super::activity::{ProcessCpu, parse_process_stat};

    let line = "1 (init) S 0 1 1 0 -1 4194560 0 0 0 0 5 5 -1 -1";
    assert_eq!(
        parse_process_stat(line, TICKS_PER_SECOND),
        Some(ProcessCpu {
            pid: 1,
            parent: 0,
            process_group: 1,
            cpu_time: Duration::from_millis(100),
        })
    );
}

#[cfg(target_os = "linux")]
#[test]
fn parse_process_stat_reads_the_kernels_own_rendering() {
    use super::activity::parse_process_stat;

    let own = fs::read_to_string("/proc/self/stat").expect("read this process's stat");
    let stat = parse_process_stat(&own, TICKS_PER_SECOND).expect("the kernel's format parses");
    assert_eq!(stat.pid, std::process::id());
    assert_eq!(stat.parent, std::os::unix::process::parent_id());
    assert!(stat.process_group > 0, "got {stat:?}");
}

/// The rate the fixture stat lines are read at: 10 ms per tick.
const TICKS_PER_SECOND: NonZeroU64 = NonZeroU64::new(100).unwrap();

fn process(pid: u32, parent: u32, process_group: u32, cpu_secs: u64) -> ProcessCpu {
    ProcessCpu {
        pid,
        parent,
        process_group,
        cpu_time: Duration::from_secs(cpu_secs),
    }
}

#[test]
fn build_cpu_time_counts_the_leader_its_group_and_its_descendants() {
    let table = [
        // The build: its leader, a `%post` step in its group, and a step
        // orphaned to init that stays in the group.
        process(100, 1, 100, 1),
        process(101, 100, 100, 2),
        process(104, 1, 100, 4),
        // A daemon the step started in a session of its own, adopted by the
        // leader, and the compiler it runs.
        process(102, 100, 102, 8),
        process(103, 102, 102, 16),
        // Unrelated processes, one of them a child of an unrelated group.
        process(200, 1, 200, 32),
        process(201, 200, 200, 64),
    ];
    assert_eq!(
        build_cpu_time(&table, 100),
        Duration::from_secs(1 + 2 + 4 + 8 + 16)
    );
    assert_eq!(build_cpu_time(&table, 200), Duration::from_secs(32 + 64));
    assert_eq!(
        build_cpu_time(&table, 999),
        Duration::ZERO,
        "a leader no process belongs to reads as no CPU time"
    );
}

#[test]
fn build_cpu_time_walks_a_parent_cycle_once() {
    // A snapshot read while pids are reused can link a process back to its
    // own descendant.
    let table = [
        process(10, 12, 10, 1),
        process(11, 10, 11, 2),
        process(12, 11, 12, 4),
    ];
    assert_eq!(build_cpu_time(&table, 10), Duration::from_secs(1 + 2 + 4));
}

/// A shell spinning in a process group of its own, the way every build is
/// spawned. Killed and reaped by the caller.
#[cfg(unix)]
fn spawn_busy_process_group() -> std::process::Child {
    use std::os::unix::process::CommandExt;

    Command::new("sh")
        .args(["-c", "while :; do :; done"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a busy shell")
}

/// A build whose CPU is burned outside its process group: a shell leading a
/// group of its own, the way every build is spawned, starts a spinning shell
/// through `setsid`, in a session and group of its own, the way a Rust
/// build's `RUSTC_WRAPPER` starts the sccache server. `setsid -f -w` forks
/// and waits, so the spinning shell stays a descendant of the leader.
#[cfg(target_os = "linux")]
struct BuildWithEscapedBusyProcess {
    leader: std::process::Child,
    escaped_pid: u32,
}

#[cfg(target_os = "linux")]
impl BuildWithEscapedBusyProcess {
    fn spawn() -> Self {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;

        let mut leader = Command::new("sh")
            .args([
                "-c",
                "setsid -f -w sh -c 'echo $$; while :; do :; done' & wait",
            ])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the build leader");
        let mut pid_line = String::new();
        BufReader::new(leader.stdout.take().expect("piped stdout"))
            .read_line(&mut pid_line)
            .expect("read the escaped process's pid");
        let escaped_pid = pid_line
            .trim()
            .parse()
            .expect("the escaped process prints its pid");
        let build = Self {
            leader,
            escaped_pid,
        };
        let escaped = fs::read_to_string(format!("/proc/{escaped_pid}/stat"))
            .ok()
            .and_then(|stat| super::activity::parse_process_stat(&stat, TICKS_PER_SECOND));
        if !escaped.is_some_and(|escaped| escaped.process_group != build.leader_pid()) {
            build.stop();
            panic!("the spinning shell must run outside the leader's group, got {escaped:?}");
        }
        build
    }

    fn leader_pid(&self) -> u32 {
        self.leader.id()
    }

    fn stop(mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", &self.escaped_pid.to_string()])
            .status();
        let _ = self.leader.kill();
        let _ = self.leader.wait();
    }
}

/// Re-reads `sample` until it shows CPU time. The loop burns CPU from its
/// first instant and the kernel accounts it in 10 ms ticks, so a reading is
/// nonzero after a tick or two; the bound only keeps a broken probe from
/// hanging the test.
#[cfg(unix)]
fn wait_for_cpu_time(mut sample: impl FnMut() -> Duration) -> Duration {
    let mut observed = Duration::ZERO;
    for _ in 0..1_000 {
        observed = sample();
        if observed > Duration::ZERO {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    observed
}

#[cfg(target_os = "linux")]
#[test]
fn build_activity_probe_sees_the_cpu_a_busy_process_group_burns() {
    let mut busy = spawn_busy_process_group();
    let probe = BuildActivityProbe::host(Vec::new(), Some(busy.id()));
    let observed = wait_for_cpu_time(|| probe.sample().cpu_time);
    let _ = busy.kill();
    let _ = busy.wait();
    assert!(
        observed > Duration::ZERO,
        "the busy group never showed CPU time"
    );

    // A leader no process belongs to reads as no CPU time at all.
    let absent = BuildActivityProbe::host(Vec::new(), Some(u32::MAX));
    assert_eq!(absent.sample().cpu_time, Duration::ZERO);
}

#[cfg(target_os = "linux")]
#[test]
fn build_activity_probe_sees_the_cpu_a_process_that_left_the_group_burns() {
    let build = BuildWithEscapedBusyProcess::spawn();
    let probe = BuildActivityProbe::host(Vec::new(), Some(build.leader_pid()));
    let observed = wait_for_cpu_time(|| probe.sample().cpu_time);
    build.stop();
    assert!(
        observed > Duration::ZERO,
        "the descendant outside the group never showed CPU time"
    );
}

#[test]
fn parse_ps_cputime_reads_bsd_and_procps_clocks() {
    // BSD `ps` (macOS): unbounded minutes, hundredths of a second.
    assert_eq!(parse_ps_cputime("0:00.05"), Some(Duration::from_millis(50)));
    assert_eq!(
        parse_ps_cputime("  123:45.67 "),
        Some(Duration::from_secs(123 * 60 + 45) + Duration::from_millis(670))
    );
    assert_eq!(
        parse_ps_cputime("0:01.5"),
        Some(Duration::from_millis(1500))
    );
    // procps (Linux): hours, and days before them.
    assert_eq!(parse_ps_cputime("00:00:05"), Some(Duration::from_secs(5)));
    assert_eq!(parse_ps_cputime("1:02:03"), Some(Duration::from_secs(3723)));
    assert_eq!(
        parse_ps_cputime("2-03:04:05"),
        Some(Duration::from_secs(2 * 86_400 + 3 * 3_600 + 4 * 60 + 5))
    );
    // A zombie's `-`, an empty field, too many fields, and a stray word.
    assert_eq!(parse_ps_cputime("-"), None);
    assert_eq!(parse_ps_cputime(""), None);
    assert_eq!(parse_ps_cputime("1:2:3:4"), None);
    assert_eq!(parse_ps_cputime("0:01.5s"), None);
}

#[test]
fn parse_ps_process_table_reads_pid_parent_group_and_time() {
    let listing = "  100     1   100   0:01.50\n  \
                   101   100   100   0:00.10\n  \
                   102   100   100   -\n\
                   header junk\n  \
                   103   101   103   00:00:02\n";
    assert_eq!(
        parse_ps_process_table(listing),
        [
            ProcessCpu {
                pid: 100,
                parent: 1,
                process_group: 100,
                cpu_time: Duration::from_millis(1_500),
            },
            ProcessCpu {
                pid: 101,
                parent: 100,
                process_group: 100,
                cpu_time: Duration::from_millis(100),
            },
            ProcessCpu {
                pid: 103,
                parent: 101,
                process_group: 103,
                cpu_time: Duration::from_secs(2),
            },
        ]
    );
}

/// The `ps` listing is read on macOS in production and on Linux here, where
/// procps renders the same columns in its own clock format.
#[cfg(unix)]
#[test]
fn ps_build_cpu_time_sees_the_cpu_a_busy_process_group_burns() {
    use super::activity::ps_build_cpu_time;

    let mut busy = spawn_busy_process_group();
    let observed = wait_for_cpu_time(|| ps_build_cpu_time(busy.id()));
    let _ = busy.kill();
    let _ = busy.wait();
    assert!(
        observed > Duration::ZERO,
        "the busy group never showed CPU time"
    );
    assert_eq!(ps_build_cpu_time(u32::MAX), Duration::ZERO);
}

#[cfg(target_os = "linux")]
#[test]
fn ps_build_cpu_time_sees_the_cpu_a_process_that_left_the_group_burns() {
    use super::activity::ps_build_cpu_time;

    let build = BuildWithEscapedBusyProcess::spawn();
    let observed = wait_for_cpu_time(|| ps_build_cpu_time(build.leader_pid()));
    build.stop();
    assert!(
        observed > Duration::ZERO,
        "the descendant outside the group never showed CPU time"
    );
}

#[test]
fn parse_guest_activity_reads_the_cache_size_and_the_builds_cpu_time() {
    // The cache size, the guest's tick rate, the leader, then the guest's
    // stat lines: the build's leader, a compiler it runs in a group of its
    // own, and an unrelated process.
    let stat_lines = "40 (sh) S 1 40 40 0 -1 0 0 0 0 0 1 1 0 0\n\
                      41 (rustc) R 40 41 41 0 -1 0 0 0 0 0 150 50 0 0\n\
                      90 (sshd) S 1 90 90 0 -1 0 0 0 0 0 500 500 0 0\n";
    assert_eq!(
        parse_guest_activity(&format!("2048\n100\n40\n{stat_lines}")),
        BuildActivity {
            bytes_on_disk: 2048,
            cpu_time: Duration::from_millis(2_020),
        }
    );
    // The guest's own tick rate converts the counters.
    assert_eq!(
        parse_guest_activity(&format!("2048\n250\n40\n{stat_lines}")).cpu_time,
        Duration::from_millis(808)
    );
    // An unknown tick rate reads at the default.
    assert_eq!(
        parse_guest_activity(&format!("2048\n\n40\n{stat_lines}")).cpu_time,
        Duration::from_millis(2_020)
    );
    // A missing cache dir prints an empty first line; the CPU still counts.
    assert_eq!(
        parse_guest_activity(&format!("\n100\n40\n{stat_lines}")),
        BuildActivity {
            bytes_on_disk: 0,
            cpu_time: Duration::from_millis(2_020),
        }
    );
    // No leader recorded (the build has not started, or is over), or a zero
    // one, reads as no CPU time; the cache is still measured.
    for leader in ["", "0"] {
        assert_eq!(
            parse_guest_activity(&format!("2048\n100\n{leader}\n{stat_lines}")),
            BuildActivity {
                bytes_on_disk: 2048,
                cpu_time: Duration::ZERO,
            },
            "leader line {leader:?}"
        );
    }
    // Garbage and nothing at all both read as zero.
    assert_eq!(
        parse_guest_activity("du: cannot read\n"),
        BuildActivity::default()
    );
    assert_eq!(parse_guest_activity(""), BuildActivity::default());
}

#[test]
fn lima_guest_activity_argv_passes_the_pgid_file_as_the_script_parameter() {
    use super::lima::{GUEST_ACTIVITY_SCRIPT, lima_guest_activity_argv};

    let argv = lima_guest_activity_argv(Some(Path::new("/tmp/peppy/pgids/build-1.pgid")));
    assert_eq!(
        argv,
        [
            "sh",
            "-c",
            GUEST_ACTIVITY_SCRIPT,
            "sh",
            "/tmp/peppy/pgids/build-1.pgid"
        ]
    );

    let argv = lima_guest_activity_argv(None);
    assert_eq!(
        argv[4], "",
        "with no pgid file the parameter is empty, so `cat` finds nothing"
    );
}

/// The guest activity script is plain POSIX shell over `/proc`, `du` and
/// `getconf`, so a Linux host runs it exactly as the Lima guest does.
#[cfg(target_os = "linux")]
#[test]
fn guest_activity_script_reports_the_cache_size_and_the_builds_cpu_time() {
    use super::lima::lima_guest_activity_argv;

    let cache = TempDir::new().expect("tempdir");
    fs::write(cache.path().join("blob"), vec![0u8; 4096]).expect("write");
    let pgids = TempDir::new().expect("tempdir");
    let pgid_file = pgids.path().join("build.pgid");
    let build = BuildWithEscapedBusyProcess::spawn();
    fs::write(&pgid_file, build.leader_pid().to_string()).expect("write the pgid file");

    let run_script = |pgid_file: Option<&Path>| {
        let argv = lima_guest_activity_argv(pgid_file);
        let out = Command::new(&argv[0])
            .args(&argv[1..])
            .env("APPTAINER_CACHEDIR", cache.path())
            .stdin(Stdio::null())
            .output()
            .expect("run the guest activity script");
        assert!(
            out.status.success(),
            "the script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_guest_activity(&String::from_utf8_lossy(&out.stdout))
    };

    let mut reading = BuildActivity::default();
    let observed = wait_for_cpu_time(|| {
        reading = run_script(Some(&pgid_file));
        reading.cpu_time
    });
    build.stop();
    assert!(
        reading.bytes_on_disk >= 4096,
        "`du -sb` must count the blob, got {}",
        reading.bytes_on_disk
    );
    assert!(
        observed > Duration::ZERO,
        "the build's descendant outside its group never showed CPU time"
    );

    // Without a pgid file the cache is still measured and the CPU is 0.
    let reading = run_script(None);
    assert!(
        reading.bytes_on_disk >= 4096,
        "got {}",
        reading.bytes_on_disk
    );
    assert_eq!(reading.cpu_time, Duration::ZERO);
}
