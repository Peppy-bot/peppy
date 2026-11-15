use super::absent_bind_paths::AbsentBindPaths;
use super::activity::{
    BuildActivity, BuildActivityProbe, ProcessCpu, build_cpu_time, parse_guest_activity,
    parse_ps_cputime, parse_ps_process_table,
};
#[cfg(target_os = "linux")]
use super::facade::{
    ApparmorProfileRef, SetupFix, SetupVerdict, apparmor_profile_ref, check_setup_status,
    profile_file_names_programs, setup_fix, shell_escape_single_quoted, unmet_prerequisites_error,
};
use super::facade::{Apptainer, Backend, GuestSignal, is_uri, passes_contain, prepare_scratch_dir};
#[cfg(target_os = "linux")]
use super::image_files::UnpackedImages;
use super::registry_auth::{RegistryAuth, RegistryAuthSource};
#[cfg(target_os = "linux")]
use super::root_user_namespace::UserNamespace;
#[cfg(target_os = "linux")]
use super::system_programs::{FailureSeverity, Privilege, SystemProgram, SystemPrograms};
#[cfg(target_os = "linux")]
use super::unprivileged_user_namespace::UnprivilegedUserNamespace;
use crate::error::Error;
use std::assert_matches;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
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
/// this host does not meet the prerequisites of a container build.
/// Prerequisites are machine state (packages and an AppArmor profile that
/// `peppy container setup` installs), not code under test, so integration tests self-skip on an unprovisioned
/// host, mirroring the setup-status tests below and the e2e suite in
/// tests/facade.rs. Pure command-assembly and path-translation tests use
/// [`native_facade`] / [`lima_facade`] instead: they must never depend on
/// host state.
fn ready_facade() -> Option<Apptainer> {
    if !host_meets_build_prerequisites() {
        eprintln!(
            "SKIPPING: the prerequisites of a container build are not met on this host; \
             run `peppy container setup`"
        );
        return None;
    }
    Some(
        Apptainer::new()
            .expect("Apptainer::new() should succeed; apptainer is bundled at compile time"),
    )
}

/// Whether this host meets the prerequisites of apptainer that a container
/// build needs: each one that stops the daemon passes, and the kernel does
/// not refuse a user namespace. Always `true` off Linux: macOS routes through
/// the Lima VM and has no prerequisites to check.
fn host_meets_build_prerequisites() -> bool {
    #[cfg(target_os = "linux")]
    {
        let verdict = check_setup_status(&ready_apptainer_dir()).verdict;
        !verdict.fix_is_required_by_the_daemon()
            && !matches!(verdict, SetupVerdict::Unfixable { .. })
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

/// The parts of a Native-backend facade that a test controls. A part that a
/// test does not name has the value of [`native_facade`].
struct NativeFacadeFixture {
    apptainer_dir: PathBuf,
    registry_auth: RegistryAuth,
    l4t_manifest_dir: PathBuf,
    #[cfg(target_os = "linux")]
    user_namespace: UserNamespace,
    #[cfg(target_os = "linux")]
    unpacked_images: UnpackedImages,
    absent_bind_paths: AbsentBindPaths,
}

impl Default for NativeFacadeFixture {
    fn default() -> Self {
        let scratch = native_fixture_scratch();
        Self {
            apptainer_dir: PathBuf::from("/opt/apptainer"),
            registry_auth: RegistryAuth {
                source: RegistryAuthSource::Named(scratch.join("no-such-auth-file.json")),
                sanitized_file: scratch.join("registry-auth/docker-config.json"),
            },
            l4t_manifest_dir: scratch.join("no-such-l4t-manifest-dir"),
            #[cfg(target_os = "linux")]
            user_namespace: UserNamespace::ApptainerDefault,
            #[cfg(target_os = "linux")]
            unpacked_images: UnpackedImages::Never,
            absent_bind_paths: AbsentBindPaths::default(),
        }
    }
}

impl NativeFacadeFixture {
    fn build(self) -> Apptainer {
        Apptainer {
            apptainer_dir: self.apptainer_dir,
            backend: Backend::Native {
                apptainer_bin: PathBuf::from("/opt/apptainer/bin/apptainer"),
                tmp_dir: PathBuf::from(NATIVE_FIXTURE_TMP_DIR),
                registry_auth: self.registry_auth,
                l4t_manifest_dir: self.l4t_manifest_dir,
                #[cfg(target_os = "linux")]
                user_namespace: self.user_namespace,
                #[cfg(target_os = "linux")]
                unpacked_images: self.unpacked_images,
                absent_bind_paths: self.absent_bind_paths,
            },
            extra_mounts: Vec::new(),
        }
    }
}

/// Builds a Native-backend facade for command-assembly and path-translation
/// tests without touching host state. The apptainer path need not exist: these
/// tests only inspect assembled argv, translated paths, and the no-op kill path.
/// Its registry credentials come from a file that does not exist, and the
/// sanitized copy a build command writes lands in [`native_fixture_scratch`],
/// never next to the host's real credentials. Its NVIDIA L4T manifest
/// directory does not exist either, so a `--nv` command never reads the host's
/// manifest. It is the facade of a daemon whose containers need no flag to
/// get the files of their image, on a host that has the source of each bind
/// of apptainer.
fn native_facade() -> Apptainer {
    NativeFacadeFixture::default().build()
}

/// [`native_facade`] with the registry auth locations a test controls.
fn native_facade_with_registry_auth(registry_auth: RegistryAuth) -> Apptainer {
    NativeFacadeFixture {
        registry_auth,
        ..NativeFacadeFixture::default()
    }
    .build()
}

/// [`native_facade`] with the apptainer install tree and the NVIDIA L4T
/// manifest directory a test controls.
fn native_facade_with_l4t_manifest(apptainer_dir: &Path, manifest_dir: &Path) -> Apptainer {
    NativeFacadeFixture {
        apptainer_dir: apptainer_dir.to_path_buf(),
        l4t_manifest_dir: manifest_dir.to_path_buf(),
        ..NativeFacadeFixture::default()
    }
    .build()
}

/// The binds of a host that has no `/etc/localtime`, with the `bind path`
/// entries of the bundled `apptainer.conf`.
fn binds_of_a_host_without_localtime() -> AbsentBindPaths {
    AbsentBindPaths::of_config(
        "bind path = /etc/localtime\nbind path = /etc/hosts\n",
        |source| source == "/etc/localtime",
    )
}

/// [`native_facade`] on a host that has no `/etc/localtime`.
fn native_facade_on_a_host_without_localtime() -> Apptainer {
    NativeFacadeFixture {
        absent_bind_paths: binds_of_a_host_without_localtime(),
        ..NativeFacadeFixture::default()
    }
    .build()
}

/// Scratch directory shared by every [`native_facade`] in this test binary.
/// Held in a static, so it is never dropped: the shared test root reclaims it
/// once it is stale. A directory that cannot be made leaves the cell empty, so
/// each test that needs it tries again and reports the failure itself.
fn native_fixture_scratch() -> &'static Path {
    static SCRATCH: OnceLock<TempDir> = OnceLock::new();
    SCRATCH.get_or_init(super::scratch_dir).path()
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
            vm: super::lima::LimaVm::host(),
        },
        extra_mounts: Vec::new(),
    }
}

/// The arguments of an assembled command, as strings.
fn argv_of(cmd: &Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The output image and def file paths of a build under `$HOME`, which both
/// backends accept: Lima mounts the home directory into the guest.
fn build_paths_under_home() -> (PathBuf, PathBuf) {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set"));
    (home.join("test/output.sif"), home.join("test/def.def"))
}

/// Runs a binary bundled into the apptainer install with `args`, or returns
/// `None` after printing a SKIPPING diagnostic when the test environment cannot
/// execute it (a sandbox that hides the file or forbids exec). Any other spawn
/// failure fails the test.
#[cfg(target_os = "linux")]
fn run_bundled_binary_or_skip(binary: &Path, args: &[&str]) -> Option<std::process::Output> {
    let e = match Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => return Some(output),
        Err(e) => e,
    };
    if !matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    ) {
        panic!(
            "unexpected error invoking bundled binary at {:?}: {} (kind: {:?})",
            binary,
            e,
            e.kind()
        );
    }
    eprintln!(
        "SKIPPING: cannot invoke bundled binary at {binary:?}: {e} (likely a sandboxed test env)"
    );
    None
}

/// Whether this host's kernel restricts unprivileged user namespaces via
/// AppArmor (raw procfs flag, without the manageability gate production
/// applies on top). Used to skip the not-restricted setup-status test; the
/// restricted-state tests gate on `SetupStatus::apparmor_restricted` instead.
#[cfg(target_os = "linux")]
fn apparmor_restricts_userns() -> bool {
    fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() == "1")
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
        "last arg should be the image path, got: {args:?}"
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

    let (output, def) = build_paths_under_home();
    let cmd = facade.build(&output, &def);
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(
        args,
        [
            "build",
            "--mksquashfs-args",
            "-comp zstd -Xcompression-level 3",
            output.to_str().unwrap(),
            def.to_str().unwrap(),
        ]
    );
}

#[test]
fn a_run_and_an_exec_with_the_default_of_apptainer_pass_no_userns() {
    let facade = native_facade();

    let run_args = facade
        .run("/images/node.sif")
        .build_args()
        .expect("build_args should succeed");
    let exec_args = facade
        .exec("/images/node.sif", &["echo", "hello"])
        .build_args()
        .expect("build_args should succeed");

    assert_eq!(run_args, ["run", "/images/node.sif"]);
    assert_eq!(exec_args, ["exec", "/images/node.sif", "echo", "hello"]);
}

#[test]
fn a_run_and_an_exec_skip_the_bind_path_that_the_host_does_not_have() {
    let facade = native_facade_on_a_host_without_localtime();

    let run_args = facade
        .run("/images/node.sif")
        .build_args()
        .expect("build_args should succeed");
    let exec_args = facade
        .exec("/images/node.sif", &["echo", "hello"])
        .build_args()
        .expect("build_args should succeed");

    assert_eq!(
        run_args,
        ["run", "--no-mount", "/etc/localtime", "/images/node.sif"]
    );
    assert_eq!(
        exec_args,
        [
            "exec",
            "--no-mount",
            "/etc/localtime",
            "/images/node.sif",
            "echo",
            "hello"
        ]
    );
}

/// Apptainer binds `/etc/hosts` and `/etc/localtime` into a contained
/// container whatever its configuration holds, so a contained command skips
/// the one that the host does not have also with a configuration that names
/// neither.
#[test]
fn a_contained_exec_skips_the_bind_of_a_contained_container_that_the_host_does_not_have() {
    let facade = NativeFacadeFixture {
        absent_bind_paths: AbsentBindPaths::of_config("mount hostfs = no\n", |source| {
            source == "/etc/localtime"
        }),
        ..NativeFacadeFixture::default()
    }
    .build();

    let contained_args = facade
        .exec("/images/node.sif", &["true"])
        .raw_flag("--contain")
        .build_args()
        .expect("build_args should succeed");
    let args = facade
        .exec("/images/node.sif", &["true"])
        .build_args()
        .expect("build_args should succeed");

    assert_eq!(
        contained_args,
        [
            "exec",
            "--no-mount",
            "/etc/localtime",
            "--contain",
            "/images/node.sif",
            "true"
        ]
    );
    assert_eq!(args, ["exec", "/images/node.sif", "true"]);
}

#[test]
fn the_flags_that_contain_a_container_are_read_as_apptainer_reads_them() {
    let flags = |flags: &[&str]| -> Vec<String> { flags.iter().map(ToString::to_string).collect() };

    for contained in [
        &["--contain"][..],
        &["--containall"],
        &["-c"],
        &["-C"],
        &["--contain=true"],
        &["--contain=false", "--contain"],
        &["--cleanenv", "--containall", "--nv"],
    ] {
        assert!(
            passes_contain(&flags(contained)),
            "{contained:?} contain the container"
        );
    }
    for not_contained in [
        &[][..],
        &["--cleanenv"],
        &["--contain=false"],
        &["--contain", "--contain=false"],
        &["--containers"],
        &["--cwd", "/"],
    ] {
        assert!(
            !passes_contain(&flags(not_contained)),
            "{not_contained:?} do not contain the container"
        );
    }
}

/// A bind whose source is absent does not stop a build, so a build skips
/// none.
#[test]
fn a_build_skips_no_bind_path() {
    let facade = native_facade_on_a_host_without_localtime();

    let (output, def) = build_paths_under_home();
    let cmd = facade.build(&output, &def);
    let args = cmd.build_args().expect("build_args should succeed");

    assert!(
        !args.iter().any(|arg| arg == "--no-mount"),
        "a build should skip no bind path, got: {args:?}"
    );
}

/// The user of the Lima guest is not root, so apptainer uses a user namespace
/// there with no flag.
#[test]
fn a_lima_run_passes_no_userns() {
    let facade = lima_facade();

    let cmd = facade.run("docker://alpine");
    let args = cmd.build_args().expect("build_args should succeed");

    assert_eq!(args, ["run", "docker://alpine"]);
}

/// The commands of a daemon that is not root, where a container can write to
/// a directory on overlayfs: see `image_files`.
#[cfg(target_os = "linux")]
mod of_a_daemon_that_is_not_root_beside_overlayfs {
    use super::*;
    use crate::apptainer::image_files::fixtures::{FakeHost, HOME, daemon_that_is_not_root};

    /// [`native_facade`] of a daemon that is not root on `host`.
    fn facade_on(host: FakeHost) -> Apptainer {
        NativeFacadeFixture {
            unpacked_images: daemon_that_is_not_root(host),
            ..NativeFacadeFixture::default()
        }
        .build()
    }

    #[test]
    fn a_run_in_a_docker_container_unpacks_the_image_before_its_other_flags() {
        let facade = facade_on(FakeHost::docker_container());

        let args = facade
            .run("/images/node.sif")
            .raw_flag("--nv")
            .build_args()
            .expect("build_args should succeed");

        assert_eq!(args, ["run", "--unsquash", "--nv", "/images/node.sif"]);
    }

    #[test]
    fn an_exec_in_a_docker_container_unpacks_the_image() {
        let facade = facade_on(FakeHost::docker_container());

        let args = facade
            .exec("/images/node.sif", &["echo", "hello"])
            .build_args()
            .expect("build_args should succeed");

        assert_eq!(
            args,
            ["exec", "--unsquash", "/images/node.sif", "echo", "hello"]
        );
    }

    /// A bind source on overlayfs is enough, on a host whose other
    /// directories are not on overlayfs.
    #[test]
    fn a_run_with_a_bind_source_on_overlayfs_unpacks_the_image() {
        let facade = facade_on(FakeHost::without_overlayfs().with_overlayfs_at("/data"));

        let args = facade
            .run("/images/node.sif")
            .working_dir(Path::new("/srv/instances/node_inst"))
            .bind("/data/recordings", None, None)
            .build_args()
            .expect("build_args should succeed");

        assert_eq!(
            args,
            [
                "run",
                "--unsquash",
                "--bind",
                "/data/recordings",
                "/images/node.sif"
            ]
        );
    }

    #[test]
    fn a_run_with_a_volume_at_each_directory_mounts_the_image() {
        let host = FakeHost::docker_container().with_volumes(&["/tmp", "/var/tmp", HOME, "/srv"]);
        let facade = facade_on(host);

        let args = facade
            .run("/images/node.sif")
            .working_dir(Path::new("/srv/instances/node_inst"))
            .build_args()
            .expect("build_args should succeed");

        assert_eq!(args, ["run", "/images/node.sif"]);
    }

    /// A build unpacks no image: apptainer makes the image in a directory of
    /// the disk.
    #[test]
    fn a_build_in_a_docker_container_passes_no_unpack_flag() {
        let facade = facade_on(FakeHost::docker_container());

        let (output, def) = build_paths_under_home();
        let args = facade
            .build(&output, &def)
            .build_args()
            .expect("build_args should succeed");

        assert!(
            !args.iter().any(|arg| arg == "--unsquash"),
            "a build should unpack no image, got: {args:?}"
        );
    }
}

/// The commands of a daemon that is the real root, which only a Linux host
/// has: see `root_user_namespace`.
#[cfg(target_os = "linux")]
mod of_a_root_daemon {
    use super::*;
    use crate::apptainer::facade::SQUASHFS_COMPRESSION;
    use crate::apptainer::root_user_namespace::fixtures::{
        FakeHost, PRIVATE_HOME, root_daemon_on_a_kernel_without_zstd, sif_packed_with,
    };
    use crate::apptainer::root_user_namespace::{COMPRESSION_OF_BUILT_IMAGES, SquashfsCompression};

    /// [`native_facade`] of a daemon that is the real root on `host`, with a
    /// kernel that mounts no zstd squashfs.
    fn facade_of_a_root_daemon(host: FakeHost) -> Apptainer {
        NativeFacadeFixture {
            user_namespace: root_daemon_on_a_kernel_without_zstd(host),
            ..NativeFacadeFixture::default()
        }
        .build()
    }

    /// An image file packed with `compression`, in a scratch directory that
    /// lives as long as the returned guard.
    fn image_packed_with(compression: SquashfsCompression) -> (TempDir, String) {
        let scratch = crate::apptainer::scratch_dir();
        let image = scratch.path().join("node.sif");
        fs::write(&image, sif_packed_with(compression)).expect("write the image");
        let image = image
            .to_str()
            .expect("the scratch path is valid UTF-8")
            .to_string();
        (scratch, image)
    }

    /// Whether `error` says that root cannot enter `closed_dir`.
    fn names_closed_dir(error: &Error, closed_dir: &str) -> bool {
        matches!(
            error,
            Error::ClosedToRootInUserNamespace { closed_dir: named, .. } if named == closed_dir
        )
    }

    #[test]
    fn a_run_of_an_image_that_the_kernel_cannot_mount_passes_userns_before_its_other_flags() {
        let facade = facade_of_a_root_daemon(FakeHost::default());
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let cmd = facade.run(&image).raw_flag("--nv").args(&["--port", "1"]);
        let args = cmd.build_args().expect("build_args should succeed");

        assert_eq!(args, ["run", "--userns", "--nv", &image, "--port", "1"]);
    }

    #[test]
    fn an_exec_of_an_image_that_the_kernel_cannot_mount_passes_userns() {
        let facade = facade_of_a_root_daemon(FakeHost::default());
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let cmd = facade.exec(&image, &["echo", "hello"]);
        let args = cmd.build_args().expect("build_args should succeed");

        assert_eq!(args, ["exec", "--userns", &image, "echo", "hello"]);
    }

    /// The container of such an image keeps the capabilities of the real
    /// root.
    #[test]
    fn a_run_of_an_image_that_the_kernel_mounts_passes_no_userns() {
        let facade = facade_of_a_root_daemon(FakeHost::default());
        let (_scratch, image) = image_packed_with(SquashfsCompression::Gzip);

        let cmd = facade.run(&image);
        let args = cmd.build_args().expect("build_args should succeed");

        assert_eq!(args, ["run", &image]);
    }

    /// A build mounts no image, so it needs no user namespace for one.
    #[test]
    fn a_build_passes_no_userns() {
        let facade = facade_of_a_root_daemon(FakeHost::default());

        let (output, def) = build_paths_under_home();
        let cmd = facade.build(&output, &def);
        let args = cmd.build_args().expect("build_args should succeed");

        assert!(
            !args.iter().any(|arg| arg == "--userns"),
            "a build should not ask for a user namespace, got: {args:?}"
        );
    }

    #[test]
    fn a_run_and_an_exec_from_an_installation_out_of_reach_name_the_closed_directory() {
        let facade = facade_of_a_root_daemon(FakeHost::default().with_dir("/opt", PRIVATE_HOME));
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let run_error = facade
            .run(&image)
            .build_args()
            .expect_err("a run cannot start its container");
        let exec_error = facade
            .exec(&image, &["echo", "hello"])
            .build_args()
            .expect_err("an exec cannot start its container");

        for error in [run_error, exec_error] {
            assert!(
                names_closed_dir(&error, "/opt"),
                "the error names the closed directory: {error}"
            );
        }
    }

    #[test]
    fn a_run_with_a_bind_source_out_of_reach_names_the_closed_directory() {
        let facade =
            facade_of_a_root_daemon(FakeHost::default().with_dir("/home/user", PRIVATE_HOME));
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let error = facade
            .run(&image)
            .bind("/srv/data", Some("/data"), None)
            .bind("/home/user/.peppy/logs", None, Some("ro"))
            .build_args()
            .expect_err("a run cannot start its container");

        assert!(
            names_closed_dir(&error, "/home/user"),
            "the error names the closed directory: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("the bind source /home/user/.peppy/logs"),
            "the error names the bind source: {error}"
        );
    }

    #[test]
    fn a_run_with_a_working_directory_out_of_reach_names_the_closed_directory() {
        let facade =
            facade_of_a_root_daemon(FakeHost::default().with_dir("/home/user", PRIVATE_HOME));
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let error = facade
            .run(&image)
            .working_dir(Path::new("/home/user/.peppy/instances/node"))
            .build_args()
            .expect_err("a run cannot start its container");

        assert!(
            names_closed_dir(&error, "/home/user"),
            "the error names the closed directory: {error}"
        );
    }

    /// An exec that names no working directory runs in the one of the
    /// daemon.
    #[test]
    fn an_exec_from_a_working_directory_of_the_daemon_out_of_reach_is_refused() {
        let host = FakeHost::default()
            .with_dir("/home/user", PRIVATE_HOME)
            .with_current_dir("/home/user");
        let facade = facade_of_a_root_daemon(host);
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let error = facade
            .exec(&image, &["true"])
            .build_args()
            .expect_err("an exec cannot start its container");

        assert!(
            names_closed_dir(&error, "/home/user"),
            "the error names the closed directory: {error}"
        );
    }

    /// A build uses no user namespace, so it works where no container of
    /// its image can start.
    #[test]
    fn a_build_from_an_installation_out_of_reach_is_assembled() {
        let facade = facade_of_a_root_daemon(FakeHost::default().with_dir("/opt", PRIVATE_HOME));

        let (output, def) = build_paths_under_home();
        let cmd = facade.build(&output, &def);
        let args = cmd.build_args().expect("build_args should succeed");

        assert_eq!(args.first().map(String::as_str), Some("build"));
    }

    #[test]
    fn a_run_that_forces_the_user_namespace_also_skips_the_absent_bind_path() {
        let facade = NativeFacadeFixture {
            user_namespace: root_daemon_on_a_kernel_without_zstd(FakeHost::default()),
            absent_bind_paths: binds_of_a_host_without_localtime(),
            ..NativeFacadeFixture::default()
        }
        .build();
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);

        let cmd = facade.run(&image);
        let args = cmd.build_args().expect("build_args should succeed");

        assert_eq!(
            args,
            ["run", "--userns", "--no-mount", "/etc/localtime", &image]
        );
    }

    /// The daemon tells its operator about the images that peppy builds, so
    /// it names the compression that a build packs them with.
    #[test]
    fn a_build_packs_with_the_compression_that_the_daemon_names_for_built_images() {
        assert!(
            SQUASHFS_COMPRESSION
                .starts_with(&format!("-comp {} ", COMPRESSION_OF_BUILT_IMAGES.name())),
            "{SQUASHFS_COMPRESSION} packs with {}",
            COMPRESSION_OF_BUILT_IMAGES.name()
        );
    }
}

/// A node's own `--mksquashfs-args`, passed as a raw flag, comes after the
/// default compression, so apptainer, which reads the last one, packs with it.
#[test]
fn a_build_flag_for_mksquashfs_comes_after_the_default_compression() {
    let facade = native_facade();

    let (output, def) = build_paths_under_home();
    let args = facade
        .build(&output, &def)
        .raw_flag("--mksquashfs-args")
        .raw_flag("-comp gzip")
        .build_args()
        .expect("build_args should succeed");

    let compressions: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| *arg == "--mksquashfs-args")
        .map(|(position, _)| args[position + 1].as_str())
        .collect();
    assert_eq!(
        compressions,
        ["-comp zstd -Xcompression-level 3", "-comp gzip"]
    );
}

/// The compression value is one argv element on Lima too: it reaches the
/// guest apptainer as a positional parameter, never re-tokenized by a shell.
#[test]
fn a_lima_build_passes_the_compression_as_one_argument() {
    let facade = lima_facade();

    let (output, def) = build_paths_under_home();
    let std_cmd = facade
        .build(&output, &def)
        .into_std_command()
        .expect("command should assemble");

    let args = argv_of(&std_cmd);
    let flag = args
        .iter()
        .position(|a| a == "--mksquashfs-args")
        .expect("the guest argv must carry the compression flag");
    assert_eq!(args[flag + 1], "-comp zstd -Xcompression-level 3");
}

#[test]
fn test_apptainer_env_sets_process_env_on_native_build() {
    let facade = native_facade();

    let (output, def) = build_paths_under_home();
    let cmd = facade
        .build(&output, &def)
        .apptainer_env("RUSTC_WRAPPER", "/peppy-cache/sccache");

    let args = cmd.build_args().expect("build_args should succeed");
    assert!(
        args.iter().all(|a| !a.contains("RUSTC_WRAPPER")),
        "apptainer_env must not appear in argv, got: {args:?}"
    );

    let std_cmd = cmd.into_std_command().expect("command should assemble");
    let env: Vec<_> = std_cmd.get_envs().collect();
    assert!(
        env.contains(&(
            std::ffi::OsStr::new("APPTAINERENV_RUSTC_WRAPPER"),
            Some(std::ffi::OsStr::new("/peppy-cache/sccache"))
        )),
        "expected APPTAINERENV_RUSTC_WRAPPER in process env, got: {env:?}"
    );
}

/// Under Lima, no value rides a command line, which the process list shows:
/// the guest argv holds the fixed script that reads the pairs from standard
/// input, then apptainer and its arguments, and the host process environment,
/// which does not cross `limactl shell`, holds no pair either.
#[test]
fn test_apptainer_env_rides_no_command_line_on_lima_build() {
    let facade = lima_facade();

    let (output, def) = build_paths_under_home();
    let std_cmd = facade
        .build(&output, &def)
        .apptainer_env("RUSTC_WRAPPER", "/peppy-cache/sccache")
        .into_std_command()
        .expect("command should assemble");

    let args = argv_of(&std_cmd);
    assert!(
        args.iter().all(|arg| !arg.contains("/peppy-cache/sccache")),
        "no value may ride the argv, got: {args:?}"
    );
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .expect("limactl argv must contain the -- separator");
    let guest = &args[separator + 1..];
    let script = guest
        .iter()
        .position(|arg| arg.contains("read -r pair"))
        .expect("the guest argv runs the script that reads the pairs");
    assert_eq!(guest[script - 2], "sh");
    assert_eq!(guest[script - 1], "-c");
    assert_eq!(
        guest[script + 1],
        "sh",
        "the `$0` placeholder of the script"
    );
    assert_eq!(guest[script + 2], "/tmp/peppy/apptainer/bin/apptainer");
    assert_eq!(guest[script + 3], "build");
    assert!(
        std_cmd
            .get_envs()
            .all(|(key, _)| !key.to_string_lossy().starts_with("APPTAINERENV_")),
        "process env on limactl does not reach the guest and must stay unset"
    );
}

/// Under Lima, an entry that would break a line of the pairs that standard
/// input carries is refused when the command is assembled, naming it.
#[test]
fn a_lima_entry_that_would_break_a_line_is_refused() {
    let facade = lima_facade();
    let (output, def) = build_paths_under_home();

    for (key, value) in [("SPLIT", "a\nINJECTED=b"), ("A=B", "c"), ("", "d")] {
        let error = facade
            .build(&output, &def)
            .apptainer_env(key, value)
            .into_std_command()
            .expect_err("an entry that breaks a line is refused");
        assert_matches!(&error, Error::GuestEnvEntryUnsendable { key: refused } if refused == key,
            "{key}: {error}"
        );
    }
}

/// The guest script exports each pair that its standard input carries,
/// whole, and the command it then runs finds its standard input at its end.
/// Driven through the system `sh`, `env` and `cat`, so the test writes no
/// program and runs no Lima VM.
#[test]
fn the_guest_script_exports_the_pairs_of_its_standard_input_and_runs_the_command() {
    let pairs = [
        (
            "WORDS".to_string(),
            "simulation=mujoco,alpha.robot_commander=mcp_commander".to_string(),
        ),
        (
            "GEMINI_API_KEY".to_string(),
            "key-with_all:the/safe.chars@+%=,".to_string(),
        ),
        ("EMPTY".to_string(), String::new()),
    ];
    let run = |program: &str| {
        let lines = super::lima::guest_env_lines(&pairs).expect("the pairs break no line");
        Command::new(super::lima::GUEST_SHELL)
            .args(super::lima::lima_guest_env_args(Path::new(program), &[]))
            .stdin(super::lima::stdin_carrying(lines).expect("a pipe"))
            .stderr(Stdio::inherit())
            .output()
            .expect("the system sh runs")
    };

    let printed = run("env");
    assert!(printed.status.success(), "{printed:?}");
    let printed = String::from_utf8(printed.stdout).expect("utf-8");
    for (key, value) in &pairs {
        let line = format!("APPTAINERENV_{key}={value}");
        assert!(
            printed.lines().any(|printed_line| printed_line == line),
            "{line} in:\n{printed}"
        );
    }

    let leftover = run("cat");
    assert!(leftover.status.success(), "{leftover:?}");
    assert!(
        leftover.stdout.is_empty(),
        "the command reads no pair: {:?}",
        String::from_utf8_lossy(&leftover.stdout)
    );
}

/// The standard input that carries a payload delivers all of it, also when
/// the payload is larger than a pipe buffer.
#[test]
fn the_standard_input_carries_a_payload_larger_than_a_pipe_buffer() {
    let payload: String = (0..64_000).map(|line| format!("K{line}=v\n")).collect();
    assert!(payload.len() > 512 * 1024, "{}", payload.len());

    let echoed = Command::new("cat")
        .stdin(super::lima::stdin_carrying(payload.clone()).expect("a pipe"))
        .output()
        .expect("the system cat runs");

    assert!(echoed.status.success(), "{echoed:?}");
    assert!(
        echoed.stdout == payload.as_bytes(),
        "the payload arrives whole"
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
    assert_eq!(bind_count, 2, "should have 2 --bind flags, got: {args:?}");
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
        "bind spec should have src:dest format, got: {bind_spec}"
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
        "bind spec should have src:dest:opts format, got: {bind_spec}"
    );
}

/// Options need a destination in the spec to ride on, so a bind that mirrors
/// its source writes the source out as the destination instead of dropping
/// the options: a read-only bind must not silently become a writable one.
#[test]
fn test_bind_without_dest_keeps_its_opts() {
    let facade = native_facade();

    let cmd = facade.run("image.sif").bind("/data", None, Some("ro"));
    let args = cmd.build_args().expect("build_args should succeed");

    let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
    assert_eq!(
        args[bind_idx + 1],
        "/data:/data:ro",
        "bind spec should mirror the source and keep the options, got: {args:?}"
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
        assert_matches!(&error, Error::BindPathHoldsDelimiter { delimiter, .. }
                if *delimiter == ':' || *delimiter == ',',
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
        "lima_shell_extra_args should not appear in apptainer args: {args:?}"
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
    let args = argv_of(&cmd);

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
        "should contain --force: {args:?}"
    );
}

/// A Jetson as a `--nv` command sees it: an install tree holding apptainer's
/// `--nv` library list, and an L4T manifest naming one shared library.
struct JetsonFixture {
    scratch: TempDir,
}

impl JetsonFixture {
    const STOCK_LIST: &'static str = "libcuda.so\nlibEGL_nvidia.so\n";

    fn new() -> Self {
        let scratch = super::scratch_dir();
        let fixture = Self { scratch };
        fs::create_dir_all(fixture.nvliblist().parent().unwrap()).unwrap();
        fs::write(fixture.nvliblist(), Self::STOCK_LIST).unwrap();
        fs::create_dir_all(fixture.manifest_dir()).unwrap();
        fs::write(
            fixture.manifest_dir().join("drivers.csv"),
            "lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvcucompat.so\n",
        )
        .unwrap();
        fixture
    }

    fn apptainer_dir(&self) -> PathBuf {
        self.scratch.path().join("apptainer")
    }

    fn manifest_dir(&self) -> PathBuf {
        self.scratch.path().join("host-files-for-container.d")
    }

    fn nvliblist(&self) -> PathBuf {
        self.apptainer_dir().join("etc/apptainer/nvliblist.conf")
    }

    fn facade(&self) -> Apptainer {
        native_facade_with_l4t_manifest(&self.apptainer_dir(), &self.manifest_dir())
    }

    fn list(&self) -> String {
        fs::read_to_string(self.nvliblist()).unwrap()
    }
}

#[test]
fn a_run_with_nv_puts_the_l4t_manifest_libraries_into_the_nv_list_first() {
    let jetson = JetsonFixture::new();
    let facade = jetson.facade();

    facade
        .run("image.sif")
        .raw_flag("--nv")
        .into_std_command()
        .expect("command should assemble");

    let list = jetson.list();
    assert!(list.starts_with(JetsonFixture::STOCK_LIST), "{list}");
    assert!(
        list.lines().any(|line| line == "libnvcucompat.so"),
        "the manifest library is missing from the list: {list}"
    );
}

#[test]
fn a_run_with_nv_set_to_true_updates_the_nv_list_too() {
    let jetson = JetsonFixture::new();
    let facade = jetson.facade();

    facade
        .run("image.sif")
        .raw_flag("--nv=true")
        .into_std_command()
        .expect("command should assemble");

    assert_ne!(jetson.list(), JetsonFixture::STOCK_LIST);
}

#[test]
fn a_run_without_nv_leaves_the_nv_list_alone() {
    for flags in [
        &["--writable-tmpfs"][..],
        &["--nvccli"],
        &["--nv=false"],
        &["--nv", "--nv=false"],
    ] {
        let jetson = JetsonFixture::new();
        let facade = jetson.facade();

        let mut cmd = facade.run("image.sif");
        for flag in flags {
            cmd = cmd.raw_flag(flag);
        }
        cmd.into_std_command().expect("command should assemble");

        assert_eq!(jetson.list(), JetsonFixture::STOCK_LIST, "for {flags:?}");
    }
}

#[test]
fn a_run_with_nv_fails_before_spawning_when_the_nv_list_cannot_be_updated() {
    let jetson = JetsonFixture::new();
    fs::remove_file(jetson.nvliblist()).unwrap();
    let facade = jetson.facade();

    let error = facade
        .run("image.sif")
        .raw_flag("--nv")
        .into_std_command()
        .expect_err("a list that cannot be read fails the command");

    assert_matches!(&error, Error::NvliblistUpdateFailed { path, .. }
            if *path == jetson.nvliblist().display().to_string(),
        "{error:?}"
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
        !apptainer_bin.is_empty(),
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
        "Expected 'bin/apptainer not found' in error, got: {err_msg}"
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
        "Expected 'bin/apptainer not found' in error, got: {err_msg}"
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
        "Expected version string containing 'apptainer', got: {v}"
    );
    eprintln!("apptainer version: {v}");
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
        "Error should mention Lima VM inaccessibility, got: {err_msg}"
    );
    assert!(
        err_msg.contains("/opt/external/file.def"),
        "Error should include the offending path, got: {err_msg}"
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

/// A host runtime path stays rejected under Lima (passing it through to the
/// guest would only defer the failure to Apptainer), but it must be rejected
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
/// the Lima YAML (restarting the VM to mount the Mac's `/run/user` over the
/// guest's own) and then whitelisted in `extra_mounts`.
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
/// advice does apply to it: put it under `$HOME`, or declare it as a mount.
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
                "Lima peppy instance should be Running after construction, got: '{status}'"
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

    // On systems with each program of the distribution, without AppArmor
    // restrictions and with a kernel that does not refuse a user namespace,
    // everything should pass.
    if status.programs.severity().is_none()
        && !status.apparmor_restricted
        && status.user_namespace.refusal().is_none()
    {
        assert_eq!(
            status.verdict,
            SetupVerdict::Ready,
            "the setup should be ready when no program is missing, AppArmor does not \
             restrict and the kernel does not refuse a user namespace"
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
    if status.programs.severity().is_none() && status.user_namespace.refusal().is_none() {
        assert_eq!(status.verdict, SetupVerdict::Ready);
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

    // If the profile file exists but isn't loaded, the setup has a fix to make.
    if status.apparmor_ok && !status.apparmor_loaded {
        let script = fix_script_of(&status.verdict).expect("fix_script should exist");
        assert!(
            script.contains("apparmor_parser"),
            "fix script should include apparmor_parser to load the profile, got: {script}"
        );
    }

    // If both are true (and no program is missing, and the kernel does not
    // refuse a user namespace), the full check passes.
    if status.apparmor_ok
        && status.apparmor_loaded
        && status.programs.severity().is_none()
        && status.user_namespace.refusal().is_none()
    {
        assert_eq!(
            status.verdict,
            SetupVerdict::Ready,
            "the setup should be ready when all checks pass"
        );
    }
}

/// The script of `verdict` that fixes the checks that fail, or `None` when
/// the verdict holds none.
#[cfg(target_os = "linux")]
fn fix_script_of(verdict: &SetupVerdict) -> Option<&str> {
    verdict.fix().map(|fix| fix.script.as_str())
}

#[cfg(target_os = "linux")]
#[test]
fn check_setup_status_detects_stale_apparmor_profile_path() {
    // When the AppArmor profile names other programs than those of the
    // current installation (e.g. a previous build artifact), apparmor_ok
    // must be false so the profile gets regenerated with the correct paths.
    // Gate on the status's own apparmor_restricted (procfs flag AND
    // manageability), matching production: inside containers the raw flag can
    // read "1" while no profile can exist, which would fail the asserts below.
    let apptainer_dir = ready_apptainer_dir();
    let status = check_setup_status(&apptainer_dir);

    if !status.apparmor_restricted {
        eprintln!("SKIPPING: system does not restrict unprivileged user namespaces via AppArmor");
        return;
    }

    // Read this install's profile and check if it names the current paths.
    let profile = apparmor_profile_ref(&apptainer_dir);

    if !profile_file_names_programs(&profile) {
        assert!(
            !status.apparmor_ok,
            "apparmor_ok should be false when profile names stale paths"
        );
        let script = fix_script_of(&status.verdict).expect("fix_script should exist");
        assert!(
            script.contains(&format!("tee {}", profile.file.display())),
            "fix script should regenerate this install's profile, got: {script}"
        );
        for program_path in [&profile.apptainer_path, &profile.starter_path] {
            assert!(
                script.contains(&shell_escape_single_quoted(program_path)),
                "fix script should use the current path {program_path} (shell-escaped), \
                 got: {script}"
            );
        }
    }
}

/// A profile file passes when it names the two programs of the
/// installation, with or without lines that a person added. A file that
/// names the starter alone, another installation, or that does not exist,
/// is to write again.
#[cfg(target_os = "linux")]
#[test]
fn a_profile_file_passes_only_when_it_names_the_two_programs() {
    let scratch = tempfile::tempdir().expect("failed to create temp dir");
    let profile = ApparmorProfileRef {
        apptainer_path: "/opt/a/bin/apptainer".to_string(),
        starter_path: "/opt/a/libexec/apptainer/bin/starter".to_string(),
        name: "peppy-apptainer-0000000000000000".to_string(),
        file: scratch.path().join("peppy-apptainer-0000000000000000"),
    };
    const WRITTEN_BY_THE_SETUP: &str = "abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         @{peppy_apptainer_programs} = \"/opt/a/bin/apptainer\" \
         \"/opt/a/libexec/apptainer/bin/starter\"\n\
         \n\
         profile peppy-apptainer-0000000000000000 @{peppy_apptainer_programs} flags=(unconfined) {\n\
         \x20 userns,\n\
         }\n";

    assert!(
        !profile_file_names_programs(&profile),
        "a profile file that does not exist does not pass"
    );

    // (content of the file, whether it passes)
    let cases = [
        (WRITTEN_BY_THE_SETUP.to_string(), true),
        (
            WRITTEN_BY_THE_SETUP.replace(
                "  userns,\n",
                "  userns,\n  include if exists <local/peppy>\n",
            ),
            true,
        ),
        (
            "abi <abi/4.0>,\n\
             include <tunables/global>\n\
             \n\
             profile peppy-apptainer-0000000000000000 /opt/a/libexec/apptainer/bin/starter \
             flags=(unconfined) {\n\
             \x20 userns,\n\
             }\n"
            .to_string(),
            false,
        ),
        (WRITTEN_BY_THE_SETUP.replace("/opt/a/", "/opt/b/"), false),
        (String::new(), false),
    ];
    for (content, passes) in cases {
        fs::write(&profile.file, &content).expect("failed to write the profile file");
        assert_eq!(
            profile_file_names_programs(&profile),
            passes,
            "profile file:\n{content}"
        );
    }
}

/// The profile of an installation names the `apptainer` and `starter`
/// programs of that installation.
#[cfg(target_os = "linux")]
#[test]
fn apparmor_profile_names_the_two_programs_of_the_install() {
    let profile = apparmor_profile_ref(Path::new("/opt/peppy-a/apptainer"));

    assert_eq!(
        profile.apptainer_path,
        "/opt/peppy-a/apptainer/bin/apptainer"
    );
    assert_eq!(
        profile.starter_path,
        "/opt/peppy-a/apptainer/libexec/apptainer/bin/starter"
    );
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

/// The paths of the programs are interpolated inside the single-quoted
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

/// A profile of the fix script tests, and the commands that write and load
/// it for a user that is not root.
#[cfg(target_os = "linux")]
fn fix_script_profile() -> ApparmorProfileRef {
    ApparmorProfileRef {
        apptainer_path: "/opt/a/bin/apptainer".to_string(),
        starter_path: "/opt/a/libexec/apptainer/bin/starter".to_string(),
        name: "peppy-apptainer-0000000000000000".to_string(),
        file: PathBuf::from("/etc/apparmor.d/peppy-apptainer-0000000000000000"),
    }
}

#[cfg(target_os = "linux")]
const WRITE_PROFILE_WITH_SUDO: &str = "printf '%s\\n' 'abi <abi/4.0>,\n\
     include <tunables/global>\n\
     \n\
     @{peppy_apptainer_programs} = \"/opt/a/bin/apptainer\" \
     \"/opt/a/libexec/apptainer/bin/starter\"\n\
     \n\
     profile peppy-apptainer-0000000000000000 @{peppy_apptainer_programs} flags=(unconfined) {\n\
     \x20 userns,\n\
     }' | sudo tee /etc/apparmor.d/peppy-apptainer-0000000000000000 > /dev/null \\\n  \
     && sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer-0000000000000000";

#[cfg(target_os = "linux")]
const LOAD_PROFILE_WITH_SUDO: &str =
    "sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer-0000000000000000";

/// The checks of the programs of a user that is not root, on a machine that
/// has each program but those of `missing`.
#[cfg(target_os = "linux")]
fn programs_without(missing: &[SystemProgram]) -> SystemPrograms {
    SystemPrograms::parse(false, |program| !missing.contains(&program))
}

/// The fix script holds one step per failing prerequisite, in a fixed order,
/// and no fix exists when every prerequisite passes. A profile that is
/// missing or stale is written and loaded in one step, so a profile that only
/// needs loading gets the load alone.
#[cfg(target_os = "linux")]
#[test]
fn setup_fix_holds_a_step_for_each_failing_check() {
    const INSTALL_UIDMAP: &str = "sudo apt-get -o DPkg::Lock::Timeout=300 update \\\n  \
         && sudo apt-get -o DPkg::Lock::Timeout=300 install -y uidmap";
    let profile = fix_script_profile();
    let uidmap_then = |step: &str| format!("{INSTALL_UIDMAP} \\\n  && {step}");
    let all = programs_without(&[]);
    let no_newuidmap = programs_without(&[SystemProgram::Newuidmap]);

    // (programs, apparmor_ok, apparmor_loaded, script)
    let cases = [
        (&all, true, true, None),
        (&no_newuidmap, true, true, Some(INSTALL_UIDMAP.to_string())),
        (&all, false, true, Some(WRITE_PROFILE_WITH_SUDO.to_string())),
        (
            &all,
            false,
            false,
            Some(WRITE_PROFILE_WITH_SUDO.to_string()),
        ),
        (&all, true, false, Some(LOAD_PROFILE_WITH_SUDO.to_string())),
        (
            &no_newuidmap,
            false,
            true,
            Some(uidmap_then(WRITE_PROFILE_WITH_SUDO)),
        ),
        (
            &no_newuidmap,
            false,
            false,
            Some(uidmap_then(WRITE_PROFILE_WITH_SUDO)),
        ),
        (
            &no_newuidmap,
            true,
            false,
            Some(uidmap_then(LOAD_PROFILE_WITH_SUDO)),
        ),
    ];
    for (programs, apparmor_ok, apparmor_loaded, script) in cases {
        let fix = setup_fix(
            programs,
            apparmor_ok,
            apparmor_loaded,
            &profile,
            Privilege::Sudo,
        );

        assert_eq!(
            fix,
            script.map(|script| SetupFix {
                script,
                severity: FailureSeverity::StopsTheDaemon,
                uses_sudo: true,
            }),
            "programs={programs:?} apparmor_ok={apparmor_ok} apparmor_loaded={apparmor_loaded}"
        );
    }
}

/// Root runs each command of the fix script as it is: a machine of root
/// alone, such as a container of a bare image, has no `sudo`.
#[cfg(target_os = "linux")]
#[test]
fn setup_fix_of_root_has_no_sudo() {
    let profile = fix_script_profile();
    let programs = SystemPrograms::parse(true, |_| false);

    let fix = setup_fix(&programs, false, false, &profile, Privilege::Root)
        .expect("programs to install and a profile to write");

    assert_eq!(
        fix,
        SetupFix {
            script: format!(
                "apt-get -o DPkg::Lock::Timeout=300 update \\\n  \
                 && apt-get -o DPkg::Lock::Timeout=300 install -y squashfs-tools fuse2fs \\\n  \
                 && {}",
                WRITE_PROFILE_WITH_SUDO.replace("sudo ", "")
            ),
            severity: FailureSeverity::StopsTheDaemon,
            uses_sudo: false,
        }
    );
    assert!(!fix.script.contains("sudo"), "{}", fix.script);
}

/// The daemon starts without `fuse2fs`, so a fix that installs it alone is a
/// warning. Each other failure that the fix repairs stops the daemon.
#[cfg(target_os = "linux")]
#[test]
fn setup_fix_is_a_warning_when_fuse2fs_alone_is_missing() {
    const INSTALL_FUSE2FS: &str = "sudo apt-get -o DPkg::Lock::Timeout=300 update \\\n  \
         && sudo apt-get -o DPkg::Lock::Timeout=300 install -y fuse2fs";
    let profile = fix_script_profile();
    let no_fuse2fs = programs_without(&[SystemProgram::Fuse2fs]);
    let severity_of = |programs: &SystemPrograms, apparmor_loaded: bool| {
        setup_fix(programs, true, apparmor_loaded, &profile, Privilege::Sudo)
            .map(|fix| fix.severity)
    };

    assert_eq!(
        setup_fix(&no_fuse2fs, true, true, &profile, Privilege::Sudo),
        Some(SetupFix {
            script: INSTALL_FUSE2FS.to_string(),
            severity: FailureSeverity::Warning,
            uses_sudo: true,
        })
    );
    assert_eq!(
        severity_of(&no_fuse2fs, false),
        Some(FailureSeverity::StopsTheDaemon),
        "a profile that is not loaded stops the daemon"
    );
    assert_eq!(
        severity_of(
            &programs_without(&[SystemProgram::Mksquashfs, SystemProgram::Fuse2fs]),
            true
        ),
        Some(FailureSeverity::StopsTheDaemon),
        "a missing mksquashfs stops the daemon"
    );
}

/// The profile names each program in quotes, with a backslash before each
/// character that AppArmor reads as a pattern or as the start of a variable,
/// so that the profile attaches to that path alone. A single quote of a path stays inside the
/// single-quoted body that the fix script writes to the profile file.
#[cfg(target_os = "linux")]
#[test]
fn setup_fix_names_a_path_with_special_characters_as_it_is() {
    let profile = ApparmorProfileRef {
        apptainer_path: r#"/home/o'brien/my "apps" [x] @{y},z*?^\a/bin/apptainer"#.to_string(),
        starter_path: "/home/o'brien/a b/libexec/apptainer/bin/starter".to_string(),
        name: "peppy-apptainer-0000000000000000".to_string(),
        file: PathBuf::from("/etc/apparmor.d/peppy-apptainer-0000000000000000"),
    };

    let script = setup_fix(
        &programs_without(&[]),
        false,
        true,
        &profile,
        Privilege::Sudo,
    )
    .expect("a profile to write")
    .script;

    let programs_line = [
        "@{peppy_apptainer_programs} =",
        r#""/home/o'\''brien/my \"apps\" \[x\] \@\{y\}\,z\*\?\^\\a/bin/apptainer""#,
        r#""/home/o'\''brien/a b/libexec/apptainer/bin/starter""#,
    ]
    .join(" ");
    assert!(
        script.contains(&format!("\n{programs_line}\n")),
        "expected the line:\n{programs_line}\ngot:\n{script}"
    );
}

/// Prerequisites that fail stop the construction of the facade, and so the
/// daemon, with an error that gives the script that fixes them.
#[cfg(target_os = "linux")]
#[test]
fn unmet_prerequisites_are_an_error_that_gives_the_script() {
    const FIX_SCRIPT: &str = "sudo apt-get -o DPkg::Lock::Timeout=300 update \\\n  \
         && sudo apt-get -o DPkg::Lock::Timeout=300 install -y squashfs-tools \\\n  \
         && sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer-0000000000000000";

    let error = unmet_prerequisites_error(FIX_SCRIPT);

    assert_matches!(
        &error,
        Error::ConfigurationError(message)
            if message.starts_with("The prerequisites of Apptainer are not met.\n")
                && message.contains(
                    "  sudo apt-get -o DPkg::Lock::Timeout=300 update \\\n    \
                     && sudo apt-get -o DPkg::Lock::Timeout=300 install -y squashfs-tools \\\n    \
                     && sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer-0000000000000000\n"
                )
                && message.contains("peppy container setup")
    );
}

/// The verdict of a setup: ready when no prerequisite fails and the kernel
/// does not refuse the user namespace, fixable when a prerequisite fails, and
/// unfixable when the kernel refuses, with the fix of the prerequisites that
/// fail beside the refusal.
#[cfg(target_os = "linux")]
#[test]
fn the_verdict_of_a_setup_holds_its_fix_and_the_refusal_of_the_kernel() {
    let fix = SetupFix {
        script: "sudo apt-get install -y uidmap".to_string(),
        severity: FailureSeverity::StopsTheDaemon,
        uses_sudo: true,
    };
    let refused = UnprivilegedUserNamespace::Refused {
        answer: "unshare: unshare failed: Operation not permitted".to_string(),
    };
    let refusal = refused
        .refusal()
        .expect("a refused user namespace explains itself");

    for not_refused in [
        UnprivilegedUserNamespace::Granted,
        UnprivilegedUserNamespace::NotRequired,
        UnprivilegedUserNamespace::Untested {
            reason: "cannot run `unshare`: no such program".to_string(),
        },
    ] {
        assert_eq!(
            SetupVerdict::of(None, &not_refused),
            SetupVerdict::Ready,
            "{not_refused:?}"
        );
        assert_eq!(
            SetupVerdict::of(Some(fix.clone()), &not_refused),
            SetupVerdict::Fixable { fix: fix.clone() },
            "{not_refused:?}"
        );
    }

    for fix in [None, Some(fix)] {
        assert_eq!(
            SetupVerdict::of(fix.clone(), &refused),
            SetupVerdict::Unfixable {
                refusal: refusal.clone(),
                fix
            }
        );
    }
}

/// The daemon stops until the setup makes a fix whose failures stop it, and
/// starts with a fix that repairs a warning alone or with no fix to make.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_needs_the_fix_of_a_failure_that_stops_it() {
    let fix_of = |severity| SetupFix {
        script: "sudo apt-get install -y fuse2fs".to_string(),
        severity,
        uses_sudo: true,
    };
    let refusal = || "The kernel refuses a user namespace.".to_string();

    // (verdict, whether the daemon needs the fix)
    let cases = [
        (SetupVerdict::Ready, false),
        (
            SetupVerdict::Fixable {
                fix: fix_of(FailureSeverity::Warning),
            },
            false,
        ),
        (
            SetupVerdict::Fixable {
                fix: fix_of(FailureSeverity::StopsTheDaemon),
            },
            true,
        ),
        (
            SetupVerdict::Unfixable {
                refusal: refusal(),
                fix: None,
            },
            false,
        ),
        (
            SetupVerdict::Unfixable {
                refusal: refusal(),
                fix: Some(fix_of(FailureSeverity::Warning)),
            },
            false,
        ),
        (
            SetupVerdict::Unfixable {
                refusal: refusal(),
                fix: Some(fix_of(FailureSeverity::StopsTheDaemon)),
            },
            true,
        ),
    ];
    for (verdict, is_required) in cases {
        assert_eq!(
            verdict.fix_is_required_by_the_daemon(),
            is_required,
            "{verdict:?}"
        );
    }
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
        "apptainer cache {install_dir:?} does not exist; was ~/.peppy deleted without rebuilding?"
    );

    let sentinel = install_dir.join(crate::APPTAINER_CACHE_SENTINEL_NAME);
    assert!(
        sentinel.exists(),
        "Cache sentinel {sentinel:?} is missing; the apptainer cache may be corrupt"
    );

    let apptainer_bin = install_dir.join("bin/apptainer");
    assert!(
        apptainer_bin.exists(),
        "bin/apptainer not found in the apptainer cache {install_dir:?}"
    );
}

/// The apptainer build cache for this binary's architecture, derived at
/// runtime exactly as production code derives it.
#[cfg(target_os = "linux")]
fn linux_apptainer_cache_dir() -> Option<PathBuf> {
    super::lima::peppy_home_dir(&format!("tmp/{}", crate::APPTAINER_CACHE_DIR_NAME))
}

/// The trees build.rs copies into `OUT_DIR` ship inside release archives: their
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
            "SKIPPING: no apptainer tree in OUT_DIR at {man_dir:?}; build without \
             PEPPY_SKIP_APPTAINER_PROVISION to check it"
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

    let gocryptfs_bin = install_dir.join("libexec/apptainer/bin/gocryptfs");
    assert!(
        gocryptfs_bin.exists(),
        "gocryptfs binary missing at {gocryptfs_bin:?}; apptainer encryption support will be disabled"
    );

    // Bundle the xray helper too; same archive, useful for inspecting
    // encrypted volumes.
    let gocryptfs_xray = install_dir.join("libexec/apptainer/bin/gocryptfs-xray");
    assert!(
        gocryptfs_xray.exists(),
        "gocryptfs-xray helper missing at {gocryptfs_xray:?}"
    );

    // Sentinel encodes the version so a bump invalidates the cache.
    let sentinel = install_dir.join("libexec/apptainer/bin").join(format!(
        ".peppy-gocryptfs-version-{}",
        crate::GOCRYPTFS_VERSION
    ));
    assert!(
        sentinel.exists(),
        "gocryptfs sentinel {sentinel:?} is missing; bundled binary may be stale"
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

    let Some(output) = run_bundled_binary_or_skip(&gocryptfs_bin, &["--version"]) else {
        return;
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
        "bundled gocryptfs version mismatch: expected to find {expected:?} in {stdout:?}"
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
        "gocryptfs should be bundled at {expected:?}"
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
        "squashfuse_ll missing at {squashfuse_bin:?}; apptainer would extract every SIF into --tmpdir \
         instead of FUSE-mounting it"
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
    let Some(output) = run_bundled_binary_or_skip(&squashfuse_bin, &[]) else {
        return;
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
        "bundled squashfuse version mismatch: expected to find {expected:?} in {banner:?}"
    );
    assert!(
        banner.contains("-o uid="),
        "bundled squashfuse_ll does not advertise `-o uid=`, so apptainer will not map file \
         ownership into the container; banner was {banner:?}"
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

/// A command with a scratch directory of its own gives apptainer that
/// directory, and not the one that the commands of the facade share.
#[test]
fn native_command_with_a_scratch_dir_of_its_own_sets_it_as_apptainer_tmpdir() {
    let facade = native_facade();
    let scratch_dir = Path::new("/opt/peppy-tmp/container_scratch/41_0");
    let cmd = facade
        .run("image.sif")
        .scratch_dir(scratch_dir)
        .into_std_command()
        .expect("native command assembly should succeed");

    assert_eq!(
        command_env(&cmd, "APPTAINER_TMPDIR"),
        Some(scratch_dir.to_path_buf())
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

/// The Lima backend ignores the scratch directory of a command, for the
/// same reason.
#[test]
fn lima_command_with_a_scratch_dir_of_its_own_does_not_set_apptainer_tmpdir() {
    let facade = lima_facade();
    let home = std::env::var("HOME").expect("HOME must be set");
    let sif = PathBuf::from(&home).join("peppy_tmpdir_test/node.sif");

    let cmd = facade
        .run(sif.to_str().expect("utf-8 sif path"))
        .scratch_dir(&PathBuf::from(&home).join("peppy_tmpdir_test/scratch"))
        .into_std_command()
        .expect("lima command assembly should succeed");

    assert_eq!(command_env(&cmd, "APPTAINER_TMPDIR"), None);
}

/// The Docker config a Docker CLI web login leaves behind: the Docker Hub
/// credential next to the CLI's own OAuth access and refresh tokens.
const WEB_LOGIN_DOCKER_CONFIG: &str = r#"{"auths": {
    "https://index.docker.io/v1/": {"auth": "cGVwcHk6ZGNrcl9wYXQ="},
    "https://index.docker.io/v1/access-token": {"auth": "cGVwcHk6and0"},
    "https://index.docker.io/v1/refresh-token": {"auth": "cGVwcHk6cmVmcmVzaA=="}
}}"#;

/// A native facade reading registry credentials from a Docker config in a
/// scratch dir that lives as long as the fixture.
struct RegistryAuthFixture {
    facade: Apptainer,
    docker_config: PathBuf,
    sanitized_file: PathBuf,
    _dir: TempDir,
}

fn native_facade_reading_docker_config(contents: &str) -> RegistryAuthFixture {
    let dir = super::scratch_dir();
    let docker_config = dir.path().join("config.json");
    fs::write(&docker_config, contents).expect("write docker config");
    let sanitized_file = dir.path().join("sanitized/docker-config.json");
    RegistryAuthFixture {
        facade: native_facade_with_registry_auth(RegistryAuth {
            source: RegistryAuthSource::Named(docker_config.clone()),
            sanitized_file: sanitized_file.clone(),
        }),
        docker_config,
        sanitized_file,
        _dir: dir,
    }
}

/// A native build is pointed at the sanitized copy, which holds the Docker Hub
/// credential and none of the OAuth token entries apptainer would otherwise
/// pick from at random.
#[test]
fn native_build_reads_registry_credentials_from_the_sanitized_copy() {
    let fixture = native_facade_reading_docker_config(WEB_LOGIN_DOCKER_CONFIG);

    let cmd = fixture
        .facade
        .build(Path::new("/work/out.sif"), Path::new("/work/node.def"))
        .into_std_command()
        .expect("native build command should assemble");

    assert_eq!(
        command_env(&cmd, "APPTAINER_AUTH_FILE"),
        Some(fixture.sanitized_file.clone())
    );
    let sanitized: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.sanitized_file).expect("read sanitized copy"))
            .expect("sanitized copy holds JSON");
    assert_eq!(
        sanitized,
        serde_json::json!({"auths": {
            "https://index.docker.io/v1/": {"auth": "cGVwcHk6ZGNrcl9wYXQ="}
        }})
    );
}

/// Running or executing a local image never reaches a registry, so it neither
/// reads the user's credentials nor depends on them being readable.
#[test]
fn native_run_and_exec_of_a_local_image_leave_registry_credentials_alone() {
    let fixture = native_facade_reading_docker_config("{not json");

    for cmd in [
        fixture.facade.run("/work/node.sif").into_std_command(),
        fixture
            .facade
            .exec("/work/node.sif", &["true"])
            .into_std_command(),
    ] {
        let cmd = cmd.expect("a local image needs no registry credentials");
        assert_eq!(command_env(&cmd, "APPTAINER_AUTH_FILE"), None);
    }
    assert!(
        !fixture.sanitized_file.exists(),
        "no sanitized copy is written"
    );
}

/// A URI image is pulled on the spot, so it gets the same credentials as a
/// build.
#[test]
fn native_run_and_exec_of_a_registry_image_read_the_sanitized_copy() {
    let fixture = native_facade_reading_docker_config(WEB_LOGIN_DOCKER_CONFIG);

    for cmd in [
        fixture.facade.run("docker://alpine:3").into_std_command(),
        fixture
            .facade
            .exec("docker://alpine:3", &["true"])
            .into_std_command(),
    ] {
        let cmd = cmd.expect("native command should assemble");
        assert_eq!(
            command_env(&cmd, "APPTAINER_AUTH_FILE"),
            Some(fixture.sanitized_file.clone())
        );
    }
}

/// A user auth file apptainer could not read fails the build before anything
/// is spawned, naming the file.
#[test]
fn native_build_fails_on_an_invalid_registry_auth_file() {
    let fixture = native_facade_reading_docker_config("{not json");

    let err = fixture
        .facade
        .build(Path::new("/work/out.sif"), Path::new("/work/node.def"))
        .into_std_command()
        .expect_err("an invalid auth file must fail command assembly");

    match err {
        Error::RegistryAuthFileInvalid { path, .. } => {
            assert_eq!(path, fixture.docker_config.display().to_string());
        }
        other => panic!("unexpected error: {other}"),
    }
}

/// Under Lima the guest apptainer reads the guest user's auth files, and host
/// process env does not reach it anyway.
#[test]
fn lima_build_does_not_set_apptainer_auth_file() {
    let facade = lima_facade();
    let (output, def) = build_paths_under_home();

    let cmd = facade
        .build(&output, &def)
        .into_std_command()
        .expect("lima build command should assemble");

    assert_eq!(command_env(&cmd, "APPTAINER_AUTH_FILE"), None);
    let args = argv_of(&cmd);
    assert!(
        !args.iter().any(|a| a.contains("APPTAINER_AUTH_FILE")),
        "the guest argv must not carry it either, got: {args:?}"
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

/// Concurrent preparation of the same scratch directory succeeds on every
/// thread. `Apptainer::new()` runs on several test threads at once (and on more
/// than one daemon thread in production), and a write probe named only after
/// the process id would have them all racing on a single path, where the first
/// `remove_file` to land wins and the others report
/// `ScratchDirUnavailable { NotFound }` against a perfectly writable directory.
#[test]
fn prepare_scratch_dir_tolerates_concurrent_preparation() {
    let tmp = TempDir::new().expect("Failed to create temp dir");
    let scratch = tmp.path().join("apptainer");

    let failures: Vec<Error> = std::thread::scope(|scope| {
        let handles: Vec<_> = Vec::from_fn(16, |_| scope.spawn(|| prepare_scratch_dir(&scratch)));
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

/// An existing but non-writable scratch directory must fail at
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
    assert_matches!(&err, Error::ScratchDirUnavailable { path, .. } if path == &scratch.display().to_string(),
        "expected ScratchDirUnavailable naming {scratch:?}, got: {err:?}"
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
    // shell`'s host-cwd propagation instead would copy the user's entire $HOME
    // into an image: the kernel canonicalizes the host cwd to `/private/var/…`,
    // the guest mount only exists at `/var/folders/…`, and Lima's own failed
    // `cd` is non-fatal, so `%files .` would run from the guest home
    // directory.
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
    let args = argv_of(&cmd);

    assert!(
        args.iter().any(|a| a == "setsid"),
        "Lima build must be wrapped in setsid, got: {args:?}"
    );
    // The pgid path is its own argv element (a positional param to the
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
    let args = argv_of(&cmd);

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
    let args = argv_of(&cmd);

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
    let args = argv_of(&cmd);

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

/// Each guest signal sends the argv of its own script: a swap would SIGKILL a
/// workload that the stop path means to ask to exit, or leave a group that a
/// cancel means to kill running.
#[test]
fn each_guest_signal_sends_its_own_argv() {
    let pgid_file = Path::new("/tmp/peppy/pgids/buildkey.pgid");
    assert_eq!(
        GuestSignal::Kill.argv(pgid_file),
        super::lima::lima_kill_pgid_argv(pgid_file)
    );
    assert_eq!(
        GuestSignal::Terminate.argv(pgid_file),
        super::lima::lima_terminate_pgid_argv(pgid_file)
    );
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
/// exist for `APPTAINER_VERSION` and `GOCRYPTFS_VERSION`.
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
fn stepping_clock(base: std::time::Instant, step: Duration) -> impl FnMut() -> std::time::Instant {
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
    assert_matches!(result, Ok(Some(status)) if status.success(),
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
    assert_matches!(
        result,
        Ok(None),
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
        if escaped.is_none_or(|escaped| escaped.process_group == build.leader_pid()) {
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

#[test]
fn the_os_family_comes_from_the_id_and_id_like_of_os_release() {
    use super::facade::{HostOsFamily, os_family_of_release};
    assert_eq!(
        os_family_of_release("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n"),
        HostOsFamily::Debian
    );
    assert_eq!(
        os_family_of_release("ID=pop\nID_LIKE=\"ubuntu debian\"\n"),
        HostOsFamily::Debian
    );
    assert_eq!(
        os_family_of_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
        HostOsFamily::Fedora
    );
    assert_eq!(os_family_of_release("ID=arch\n"), HostOsFamily::Arch);
    assert_eq!(
        os_family_of_release("ID=alpine\n"),
        HostOsFamily::OtherLinux
    );
}

#[test]
fn the_emulation_packages_name_the_emulator_and_the_firmware_of_each_family() {
    use super::facade::{HostOsFamily, emulation_packages};
    use core_node_api::encoding::ArtifactPlatform::{LinuxAmd64, LinuxArm64};
    assert_eq!(
        emulation_packages(LinuxArm64, &HostOsFamily::Debian),
        "`sudo apt install qemu-system-arm qemu-efi-aarch64`"
    );
    assert_eq!(
        emulation_packages(LinuxAmd64, &HostOsFamily::Debian),
        "`sudo apt install qemu-system-x86 ovmf`"
    );
    assert!(emulation_packages(LinuxAmd64, &HostOsFamily::MacOs).contains("brew install qemu"));
    assert!(emulation_packages(LinuxArm64, &HostOsFamily::Fedora).contains("edk2-aarch64"));
    assert!(emulation_packages(LinuxAmd64, &HostOsFamily::Arch).contains("edk2-ovmf"));
}

#[test]
fn an_emulated_vm_is_named_after_its_architecture_and_runs_under_qemu() {
    let vm = super::lima::LimaVm::emulated("aarch64");
    assert_eq!(vm.name, format!("{}-aarch64", super::lima::LIMA_INSTANCE));
    assert_eq!(
        vm.config_path(Path::new("/home/u/.lima")),
        PathBuf::from(format!(
            "/home/u/.lima/{}-aarch64/lima.yaml",
            super::lima::LIMA_INSTANCE
        ))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_vm_preparation_probe_reads_its_instance_dir_and_the_pid_its_host_agent_writes() {
    let instance = TempDir::new().expect("instance dir");
    let probe = BuildActivityProbe::vm_instance(instance.path().to_path_buf());
    assert_eq!(
        probe.sample(),
        BuildActivity::default(),
        "an empty instance dir without a host agent"
    );

    fs::write(instance.path().join("serial.log"), [0u8; 4096]).expect("write a console log");
    let mut busy = spawn_busy_process_group();
    fs::write(
        instance.path().join(super::lima::HOST_AGENT_PID_FILE),
        format!("{}\n", busy.id()),
    )
    .expect("write the pid of the host agent");
    let observed = wait_for_cpu_time(|| probe.sample().cpu_time);
    let bytes = probe.sample().bytes_on_disk;
    let _ = busy.kill();
    let _ = busy.wait();
    assert!(
        observed > Duration::ZERO,
        "the host agent never showed CPU time"
    );
    assert!(bytes >= 4096, "the console log counts: {bytes}");
}

#[test]
fn only_a_platform_built_in_a_vm_has_a_vm_preparation_probe() {
    use core_node_api::encoding::ArtifactPlatform;
    use daemon_config::consts::HOST_ARTIFACT_PLATFORM;
    let other = ArtifactPlatform::ALL
        .into_iter()
        .find(|platform| *platform != HOST_ARTIFACT_PLATFORM)
        .expect("two platforms");
    assert_eq!(
        super::lima::LimaVm::for_platform(other).map(|vm| vm.name),
        Some(format!(
            "{}-{}",
            super::lima::LIMA_INSTANCE,
            other.architecture()
        ))
    );
    assert_eq!(
        super::lima::LimaVm::for_platform(HOST_ARTIFACT_PLATFORM).is_some(),
        cfg!(target_os = "macos")
    );
}

/// Lima looks for the firmware of a guest beside the installation of QEMU
/// (Homebrew), under `~/.local`, then where the distributions put it.
#[test]
fn the_uefi_firmware_candidates_follow_the_search_of_lima() {
    let candidates = super::lima::uefi_firmware_candidates(
        "aarch64",
        Path::new("/opt/homebrew/bin/qemu-system-aarch64"),
        Some(Path::new("/home/u")),
    );
    assert_eq!(
        candidates,
        [
            "/home/u/.local/share/qemu/edk2-aarch64-code.fd",
            "/opt/homebrew/share/qemu/edk2-aarch64-code.fd",
            "/opt/homebrew/bin/share/edk2-aarch64-code.fd",
            "/usr/share/AAVMF/AAVMF_CODE.fd",
            "/usr/share/edk2/aarch64/QEMU_CODE.fd",
            "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
        ]
        .map(PathBuf::from)
    );
    let x86 = super::lima::uefi_firmware_candidates(
        "x86_64",
        Path::new("/usr/bin/qemu-system-x86_64"),
        None,
    );
    assert_eq!(x86[0], PathBuf::from("/usr/share/qemu/edk2-x86_64-code.fd"));
    assert!(x86.contains(&PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd")));
}

#[test]
fn an_emulator_without_the_firmware_of_its_guests_cannot_build() {
    use super::facade::{EmulationHost, emulator_problem};
    use core_node_api::encoding::ArtifactPlatform::LinuxArm64;
    let qemu = |program: &str| Some(PathBuf::from("/usr/bin").join(program));
    let no_qemu = |_: &str| None;
    let firmware = |path: &Path| path == Path::new("/usr/share/AAVMF/AAVMF_CODE.fd");
    let no_file = |_: &Path| false;
    let host = |find_program, is_file| EmulationHost {
        find_program,
        is_file,
        home: None,
    };

    let missing_emulator =
        emulator_problem(LinuxArm64, &host(&no_qemu, &firmware)).expect("no emulator");
    assert!(
        missing_emulator.contains("the QEMU system emulator `qemu-system-aarch64`"),
        "{missing_emulator}"
    );
    let missing_firmware =
        emulator_problem(LinuxArm64, &host(&qemu, &no_file)).expect("no firmware");
    assert!(
        missing_firmware.contains("needs the UEFI firmware of aarch64 guests"),
        "{missing_firmware}"
    );
    assert_eq!(emulator_problem(LinuxArm64, &host(&qemu, &firmware)), None);
}
