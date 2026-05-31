use super::super::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const LIMA_INSTANCE: &str = env!("LIMA_INSTANCE");
pub(crate) const LIMA_TEMPLATE: &str = env!("LIMA_TEMPLATE");
pub(crate) const MIN_LIMA_VERSION: (u32, u32, u32) = (2, 1, 0);

/// Guest-native directory for build PGID files. Lives under `/tmp/peppy` on the
/// guest's own tmpfs (alongside the synced apptainer install), never the
/// virtiofs host-home mount, so the wrapper's pgid write cannot lose a
/// mount-visibility race the way a path under `$HOME` can.
pub(crate) const GUEST_PGID_DIR: &str = "/tmp/peppy/pgids";

/// Guest-native path of a build's PGID file, keyed by a unique, filesystem-safe
/// build key (the working-dir basename). The build wrapper and the kill script
/// both derive the path from this one helper so they can never disagree.
pub(crate) fn guest_pgid_path(build_key: &str) -> PathBuf {
    PathBuf::from(GUEST_PGID_DIR).join(format!("{build_key}.pgid"))
}

/// Builds the guest-side argv (after the `limactl shell ... --` separator) that
/// runs an `apptainer build` as its own session/process-group leader and records
/// its PGID to `pgid_file` (a guest-native path from [`guest_pgid_path`]).
///
/// `setsid -w` makes `sh` the session+group leader (so its PGID equals its PID)
/// and waits for it, forwarding stdout/stderr and the exit status unchanged. The
/// `sh -c` script is a fixed constant: the pgid file, apptainer binary, and its
/// args arrive as the shell's own positional parameters (`$1`, then `$@`), so
/// they are never re-tokenized and need no shell escaping. The script `mkdir -p`s
/// the guest-native pgid dir (derived from `$1`) so the write does not race the
/// virtiofs host mount, records `$$` to `$1`, then runs apptainer as a child of
/// the same group (not via `exec`) so apptainer's `%post` children inherit the
/// group and `sh` survives to remove the pgid file and forward apptainer's exit
/// status on the normal path. On cancel, [`lima_kill_pgid_argv`] SIGKILLs the
/// whole group (`sh` + apptainer + `%post` children) from inside the VM.
pub(crate) fn lima_guest_build_argv(
    apptainer_bin: &Path,
    apptainer_args: &[&str],
    pgid_file: &Path,
) -> Vec<String> {
    // Fixed script; values follow as positional params. `sh` is the `$0`
    // placeholder so the next operand is `$1` (the pgid file) and the rest are
    // apptainer's binary and args. Derive the pgid dir from `$1`, record the
    // leader PID, then `shift` `$1` off so `"$@"` runs apptainer as a child while
    // `$pgid` keeps the path for cleanup.
    let script = "d=$(dirname \"$1\"); mkdir -p \"$d\"; echo $$ > \"$1\"; \
                  pgid=\"$1\"; shift; \"$@\"; __rc=$?; rm -f \"$pgid\"; exit $__rc";
    let mut argv = vec![
        "setsid".to_string(),
        "-w".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        pgid_file.display().to_string(),
        apptainer_bin.display().to_string(),
    ];
    argv.extend(apptainer_args.iter().map(|arg| arg.to_string()));
    argv
}

/// Guest-side argv (after the `limactl shell ... --` separator) that SIGKILLs the
/// build process group recorded at `pgid_file` by [`lima_guest_build_argv`], then
/// removes the pgid file. The `sh -c` script is a fixed constant and the pgid file
/// arrives as `$1`, so it needs no shell escaping. The negative PGID targets the
/// whole group (`sh` + apptainer + its `%post` children); the `rm -f` cleans up on
/// the cancel path, where the wrapper is SIGKILLed before it can self-clean.
/// Best-effort: a missing or already-dead group is not an error.
pub(crate) fn lima_kill_pgid_argv(pgid_file: &Path) -> Vec<String> {
    let script = "kill -KILL -\"$(cat \"$1\")\" 2>/dev/null; \
                  rm -f \"$1\" 2>/dev/null; true";
    vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        pgid_file.display().to_string(),
    ]
}

/// Build a `limactl shell <instance> --` command pre-configured with LIMA_HOME.
///
/// Callers chain additional `.arg()` / `.args()` for the guest-side command.
fn lima_shell_cmd(limactl: &Path, lima_home: &Path, instance: &str) -> Command {
    let mut cmd = Command::new(limactl);
    cmd.env("LIMA_HOME", lima_home)
        .args(["shell", instance, "--"]);
    cmd
}

/// Check that a command completed successfully, returning an error built by
/// `make_err` (which receives the trimmed stderr) on failure.
fn check_output(
    output: &std::process::Output,
    make_err: impl FnOnce(String) -> Error,
) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(make_err(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Check that the bundled Lima version meets the minimum requirement.
pub(crate) fn check_lima_version(limactl: &Path) -> Result<()> {
    let output = Command::new(limactl).arg("--version").output()?;
    validate_lima_version_output(limactl, output.status, &output.stdout, &output.stderr)
}

/// Pure validator for `limactl --version` output. Split out of
/// [`check_lima_version`] so tests can drive it without spawning a subprocess
/// (which would otherwise race with parallel tests' fork/exec — see the ETXTBSY
/// discussion in the test module).
fn validate_lima_version_output(
    limactl: &Path,
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<()> {
    if !status.success() {
        let stderr = String::from_utf8_lossy(stderr).trim().to_string();
        return Err(Error::LimaVersionCheckFailed(format!(
            "`{} --version` exited with {}{}",
            limactl.display(),
            status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        )));
    }

    let version_str = String::from_utf8_lossy(stdout);
    let found = parse_lima_version(&version_str).unwrap_or_default();

    if found < MIN_LIMA_VERSION {
        return Err(Error::LimaVersionTooOld {
            found: format!("{}.{}.{}", found.0, found.1, found.2),
            minimum: format!(
                "{}.{}.{}",
                MIN_LIMA_VERSION.0, MIN_LIMA_VERSION.1, MIN_LIMA_VERSION.2
            ),
        });
    }

    Ok(())
}

/// Query the status of a Lima instance (e.g. "Running", "Stopped").
///
/// Returns `Ok(None)` if the instance does not exist or the output is empty.
/// Returns `Err` if the command fails for reasons other than "instance not found".
fn query_instance_status(
    limactl: &Path,
    lima_home: &Path,
    instance: &str,
) -> Result<Option<String>> {
    let output = Command::new(limactl)
        .env("LIMA_HOME", lima_home)
        .args(["list", "--format", "{{.Status}}", instance])
        .output()
        .map_err(|e| Error::LimaInstanceError(format!("failed to run limactl list: {e}")))?;

    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok(if s.is_empty() { None } else { Some(s) });
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No instance matching") {
        return Ok(None);
    }

    Err(Error::LimaInstanceError(format!(
        "limactl list failed for instance '{}': {}",
        instance,
        stderr.trim()
    )))
}

/// Returns `true` if the Lima VM instance is already running and SSH-reachable.
/// This is a lightweight check that avoids booting the VM.
pub(crate) fn is_lima_instance_running(limactl: &Path, lima_home: &Path) -> bool {
    query_instance_status(limactl, lima_home, LIMA_INSTANCE)
        .ok()
        .flatten()
        .as_deref()
        == Some("Running")
        && is_ssh_alive(limactl, lima_home, LIMA_INSTANCE)
}

pub(crate) fn ensure_lima_instance(limactl: &Path, lima_home: &Path, template: &str) -> Result<()> {
    std::fs::create_dir_all(lima_home).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to create LIMA_HOME {}: {e}",
            lima_home.display()
        ))
    })?;

    let instance_status = query_instance_status(limactl, lima_home, LIMA_INSTANCE)?;

    match instance_status.as_deref() {
        Some("Running") => {
            if is_ssh_alive(limactl, lima_home, LIMA_INSTANCE) {
                return Ok(());
            }
            // VM reports Running but SSH is dead (zombie VM — VZ process crashed).
            // Force-stop and restart.
            tracing::warn!(
                "Lima {} instance reports Running but SSH is unresponsive — restarting...",
                LIMA_INSTANCE
            );
            let _ = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["stop", "--force", LIMA_INSTANCE])
                .output();

            let start = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["start", LIMA_INSTANCE])
                .output()?;

            check_output(&start, |stderr| {
                Error::LimaInstanceError(format!(
                    "failed to restart zombie Lima {} instance: {stderr}",
                    LIMA_INSTANCE
                ))
            })
        }
        Some(_) => {
            tracing::info!("Starting Lima {} instance...", LIMA_INSTANCE);
            let start = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["start", LIMA_INSTANCE])
                .output()?;

            check_output(&start, |stderr| {
                Error::LimaInstanceError(format!(
                    "failed to start Lima {} instance: {stderr}",
                    LIMA_INSTANCE
                ))
            })
        }
        None => {
            tracing::info!(
                "Creating Lima {} instance with {} (first run, may take a few minutes)...",
                LIMA_INSTANCE,
                template
            );
            let name_flag = format!("--name={}", LIMA_INSTANCE);
            let create = Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args([
                    "start",
                    &name_flag,
                    "--tty=false",
                    "--mount-writable",
                    "--memory=12",
                    template,
                ])
                .output()?;

            check_output(&create, |stderr| {
                Error::LimaInstanceError(format!(
                    "failed to create Lima {} instance: {stderr}",
                    LIMA_INSTANCE
                ))
            })
        }
    }
}

/// Quick SSH liveness probe — returns true if we can reach the guest.
fn is_ssh_alive(limactl: &Path, lima_home: &Path, instance: &str) -> bool {
    lima_shell_cmd(limactl, lima_home, instance)
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Disable AppArmor's user namespace restriction inside the Lima guest.
///
/// Ubuntu 24.04+ enables `kernel.apparmor_restrict_unprivileged_userns=1` by
/// default, which blocks Apptainer's unprivileged user namespace operations.
/// This applies the same workaround used by Lima's own `apptainer.yaml` template.
///
/// Note: `sudo` runs inside the Lima VM guest, which has passwordless sudo by default.
pub(crate) fn ensure_guest_userns(limactl: &Path, lima_home: &Path, instance: &str) -> Result<()> {
    let check = lima_shell_cmd(limactl, lima_home, instance)
        .args([
            "cat",
            "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
        ])
        .output();

    let needs_fix = match &check {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "1",
        // File doesn't exist (non-AppArmor system) or other error — skip
        _ => false,
    };

    if !needs_fix {
        return Ok(());
    }

    tracing::info!("Disabling AppArmor user namespace restriction in Lima guest...");

    let apply = lima_shell_cmd(limactl, lima_home, instance)
        .args([
            "sudo",
            "sh",
            "-c",
            "echo 'kernel.apparmor_restrict_unprivileged_userns=0' > /etc/sysctl.d/99-userns.conf && sysctl --system",
        ])
        .output()
        .map_err(|e| Error::LimaInstanceError(format!("failed to apply userns sysctl: {e}")))?;

    check_output(&apply, |stderr| {
        Error::LimaInstanceError(format!(
            "failed to disable AppArmor userns restriction in guest: {stderr}"
        ))
    })
}

/// Ensure that `newuidmap` is available inside the Lima guest.
///
/// Apptainer relies on unprivileged user namespaces via fakeroot, which
/// requires `newuidmap` (provided by the `uidmap` package on Debian/Ubuntu).
/// The base Ubuntu 24.04 template does not include it by default.
pub(crate) fn ensure_guest_uidmap(limactl: &Path, lima_home: &Path, instance: &str) -> Result<()> {
    let check = lima_shell_cmd(limactl, lima_home, instance)
        .args(["which", "newuidmap"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let already_installed = matches!(check, Ok(s) if s.success());
    if already_installed {
        return Ok(());
    }

    tracing::info!("Installing uidmap (newuidmap) in Lima guest...");

    let install = lima_shell_cmd(limactl, lima_home, instance)
        .args(["sudo", "apt-get", "install", "-y", "uidmap"])
        .output()
        .map_err(|e| Error::LimaInstanceError(format!("failed to install uidmap: {e}")))?;

    check_output(&install, |stderr| {
        Error::LimaInstanceError(format!("failed to install uidmap in guest: {stderr}"))
    })
}

/// Ensure the apptainer installation is available inside the Lima VM guest.
///
/// Syncs the host-side installation to `/tmp/peppy/apptainer/` in the guest.
/// This path lives on the guest's native writable filesystem, avoiding Lima's
/// read-only home directory mount. A version-stamped marker file avoids
/// redundant copies on subsequent invocations.
///
/// Returns the guest-side path to `bin/apptainer`.
pub(crate) fn ensure_guest_apptainer(
    host_dir: &Path,
    limactl: &Path,
    lima_home: &Path,
    instance: &str,
) -> Result<PathBuf> {
    let guest_dir = PathBuf::from(env!("GUEST_APPTAINER_DIR"));
    let guest_bin = guest_dir.join("bin/apptainer");

    let version = crate::APPTAINER_VERSION;
    let marker_name = format!(".peppy-sync-{version}");
    let marker_path = guest_dir.join(&marker_name);

    // Fast path: check if the version marker exists (sub-second limactl call).
    let marker_exists = match lima_shell_cmd(limactl, lima_home, instance)
        .args(["test", "-f"])
        .arg(&marker_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.success(),
        Err(err) => {
            tracing::warn!(
                error = %err,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to check guest marker; forcing full sync"
            );
            false
        }
    };

    if marker_exists {
        return Ok(guest_bin);
    }

    tracing::info!("Syncing apptainer installation to Lima VM guest...");

    // Remove stale installation in guest.
    let _ = lima_shell_cmd(limactl, lima_home, instance)
        .args(["rm", "-rf"])
        .arg(&guest_dir)
        .status();

    // Create the target directory in the guest.
    let mkdir = lima_shell_cmd(limactl, lima_home, instance)
        .args(["mkdir", "-p"])
        .arg(&guest_dir)
        .output()
        .map_err(|e| Error::LimaSyncFailed(format!("failed to create guest directory: {e}")))?;

    check_output(&mkdir, |stderr| {
        Error::LimaSyncFailed(format!(
            "mkdir in guest returned {}: {stderr}",
            mkdir.status
        ))
    })?;

    // Copy host installation to guest via tar pipe. `limactl copy -r` is
    // unreliable with long or special-character paths, so we tar on the host and
    // untar in the guest. The two `tar` processes are wired together in Rust (no
    // shell), so paths pass as argv and need no escaping, and a failure on either
    // side is surfaced (a shell pipe would mask the host-side `tar` exit status).
    let mut host_tar = Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(host_dir)
        .arg(".")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::LimaSyncFailed(format!("failed to start host tar: {e}")))?;

    // Hand the host tar's stdout to the guest tar's stdin. Taking it here also
    // drops our own copy of the read end so the pipe closes cleanly.
    let host_stdout = host_tar
        .stdout
        .take()
        .expect("host tar was spawned with a piped stdout");

    let guest_tar = lima_shell_cmd(limactl, lima_home, instance)
        .args(["tar", "-xf", "-", "-C"])
        .arg(&guest_dir)
        .stdin(Stdio::from(host_stdout))
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::LimaSyncFailed(format!("tar pipe to guest failed: {e}")))?;

    let host_status = host_tar
        .wait()
        .map_err(|e| Error::LimaSyncFailed(format!("failed to wait for host tar: {e}")))?;
    if !host_status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = host_tar.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(Error::LimaSyncFailed(format!(
            "host tar returned {host_status}: {}",
            stderr.trim()
        )));
    }

    check_output(&guest_tar, |stderr| {
        Error::LimaSyncFailed(format!(
            "tar pipe to guest returned {}: {stderr}",
            guest_tar.status
        ))
    })?;

    // Write the version marker so we skip the sync next time.
    match lima_shell_cmd(limactl, lima_home, instance)
        .arg("touch")
        .arg(&marker_path)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!(
                status = %status,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to write guest marker; next run may perform full sync"
            );
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                limactl = %limactl.display(),
                instance,
                marker = %marker_path.display(),
                "Failed to write guest marker; next run may perform full sync"
            );
        }
    }

    Ok(guest_bin)
}

/// Resolve an installation directory using a standard three-step fallback:
///
/// 1. Runtime override via `env_var` environment variable.
/// 2. `exe_subdir` relative to the current executable (installed layout).
/// 3. Compile-time path from build.rs (passed as `compile_time_dir`).
///
/// Returns the first directory that exists, or the error from `not_found_err`.
pub(crate) fn resolve_install_dir(
    env_var: &str,
    exe_subdir: &str,
    compile_time_dir: Option<&str>,
    compile_time_label: &str,
    not_found_err: impl FnOnce() -> Error,
) -> Result<PathBuf> {
    // 1) Runtime override via environment variable
    if let Ok(dir) = std::env::var(env_var) {
        let dir = dir.trim().to_string();
        if !dir.is_empty() {
            let path = PathBuf::from(&dir);
            if path.is_dir() {
                return Ok(path);
            }
            tracing::warn!("{env_var}={dir} does not exist or is not a directory");
        }
    }

    // 2) Relative to the current executable
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join(exe_subdir);
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    // 3) Compile-time path injected by build.rs
    if let Some(dir) = compile_time_dir {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Ok(path);
        }
        tracing::debug!("Compile-time {compile_time_label} path {dir} does not exist at runtime");
    }

    Err(not_found_err())
}

/// Resolve the Lima installation directory (contains `bin/limactl`, `share/lima/`).
///
/// Resolution order:
/// 1. `PEPPY_LIMA_DIR` environment variable
/// 2. `lima/` relative to the current executable (installed layout)
/// 3. Compile-time `LIMA_INSTALL_DIR` set by build.rs
pub(crate) fn resolve_lima_dir() -> Result<PathBuf> {
    resolve_install_dir(
        "PEPPY_LIMA_DIR",
        "lima",
        option_env!("LIMA_INSTALL_DIR"),
        "LIMA_INSTALL_DIR",
        || Error::LimaRequired,
    )
}

/// Resolve the LIMA_HOME directory for VM instance data.
///
/// Resolution order:
/// 1. `{exe_dir}/../lima-data/` — installed layout (`~/.peppy/lima-data/`).
/// 2. Compile-time `LIMA_BUILD_HOME` — reuses the build-time VM during development.
/// 3. `~/.peppy/lima-data/` — fallback for unusual layouts.
pub(crate) fn resolve_lima_home() -> Result<PathBuf> {
    // 1) Relative to the current executable: {exe_dir}/../lima-data/
    //    In the installed layout this is ~/.peppy/lima-data/.
    //    Only use this if the directory already exists (i.e. was set up by install.sh).
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("../lima-data");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }

    // 2) Compile-time build home — during `cargo test` / `cargo run` in the
    //    source tree the exe-relative path won't exist, so fall back to the
    //    LIMA_HOME used at build time (typically ~/.peppy/lima-build/).
    if let Some(dir) = option_env!("LIMA_BUILD_HOME") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Ok(path);
        }
    }

    // 3) Fallback: well-known location in the user's home
    let home = std::env::var("HOME")
        .map_err(|_| Error::ConfigurationError("HOME environment variable not set".into()))?;
    Ok(PathBuf::from(home).join(".peppy/lima-data"))
}

/// Parse a Lima version string like `"limactl version 1.1.0"` into `(major, minor, patch)`.
///
/// Returns `None` if the string cannot be parsed.
pub(crate) fn parse_lima_version(version_output: &str) -> Option<(u32, u32, u32)> {
    // Format: "limactl version X.Y.Z" or just "X.Y.Z"
    let version_str = version_output.trim().rsplit(' ').next()?;

    let mut parts = version_str.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Stop a Lima instance.
///
/// Idempotent: returns `Ok(())` if the instance is already stopped or does not
/// exist, so callers do not need to guard against these cases.
pub(crate) fn stop_instance(limactl: &Path, lima_home: &Path, instance: &str) -> Result<()> {
    stop_instance_inner(
        instance,
        || query_instance_status(limactl, lima_home, instance),
        || {
            Command::new(limactl)
                .env("LIMA_HOME", lima_home)
                .args(["stop", instance])
                .output()
                .map_err(Error::from)
        },
    )
}

/// Branch logic for [`stop_instance`], parameterized by the status query and
/// the stop-command spawner. Split out so tests can drive the full decision
/// matrix with canned closures — avoiding subprocess execution (and the
/// Linux ETXTBSY race that bites when parallel test threads fork while
/// another thread holds a writable FD to a fake `limactl` script).
fn stop_instance_inner<Q, S>(instance: &str, query_status: Q, run_stop: S) -> Result<()>
where
    Q: FnOnce() -> Result<Option<String>>,
    S: FnOnce() -> Result<std::process::Output>,
{
    let status = query_status()?;

    match status.as_deref() {
        Some("Running") => {
            tracing::info!("Stopping Lima {} instance...", instance);
            let output = run_stop()?;

            check_output(&output, |stderr| {
                Error::LimaInstanceError(format!(
                    "failed to stop Lima {} instance: {stderr}",
                    instance
                ))
            })
        }
        Some(other) => {
            tracing::info!(
                "Lima {} instance status is '{}', skipping stop",
                instance,
                other
            );
            Ok(())
        }
        None => {
            tracing::info!("Lima {} instance does not exist, skipping stop", instance);
            Ok(())
        }
    }
}

/// Ensure that the given host paths are listed as mounts in the Lima config.
///
/// Reads the Lima YAML config, checks existing mount locations, and appends
/// any missing paths as writable mounts. Returns `true` if the config was
/// modified (meaning the VM needs to be restarted to pick up the changes).
/// Top-level system directories that Lima 2.0+ rejects as guest mountPoints.
///
/// NOTE: This list is duplicated in `config-internal/src/node/types.rs`
/// (which this crate cannot depend on). Keep both in sync.
const BLOCKED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/dev", "/etc", "/home", "/opt", "/sbin", "/tmp", "/usr", "/var",
];

/// Check whether a path is a blocked top-level system mount.
///
/// Only exact top-level matches are blocked — subdirectories like `/tmp/my_app`
/// are allowed. Also handles macOS `/private/X` equivalents (e.g., `/private/tmp`
/// maps to `/tmp`).
pub(crate) fn is_blocked_system_mount(path: &str) -> bool {
    if BLOCKED_MOUNT_PATHS.contains(&path) {
        return true;
    }
    // macOS: /private/tmp -> /tmp, /private/var -> /var
    if let Some(stripped) = path.strip_prefix("/private") {
        return BLOCKED_MOUNT_PATHS.contains(&stripped);
    }
    false
}

/// Ensure that the given host paths are listed as mounts in the Lima config.
///
/// Reads the Lima YAML config, checks existing mount locations, and appends
/// any missing paths as writable mounts. Returns `true` if the config was
/// modified (meaning the VM needs to be restarted to pick up the changes).
///
/// Also performs cleanup: removes existing mount entries for paths that no longer
/// exist on the host or that are blocked system paths (which Lima would reject).
pub(crate) fn ensure_extra_mounts(config_path: &Path, paths: &[&str]) -> Result<bool> {
    let content = std::fs::read_to_string(config_path).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to read Lima config {}: {e}",
            config_path.display()
        ))
    })?;

    let mut config: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| {
        Error::LimaInstanceError(format!(
            "failed to parse Lima config {}: {e}",
            config_path.display()
        ))
    })?;

    // Get or create the mounts array
    let mounts = config
        .as_mapping_mut()
        .ok_or_else(|| Error::LimaInstanceError("Lima config is not a YAML mapping".into()))?
        .entry(serde_yaml::Value::String("mounts".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));

    let mounts_seq = mounts
        .as_sequence_mut()
        .ok_or_else(|| Error::LimaInstanceError("Lima config 'mounts' is not a sequence".into()))?;

    let mut modified = false;

    // Phase 1: Clean up stale or invalid existing mounts.
    let original_len = mounts_seq.len();
    mounts_seq.retain(|entry| {
        let Some(location) = entry.get("location").and_then(|v| v.as_str()) else {
            return true; // Keep entries without a location field
        };
        // Always keep the home mount
        if location == "~" || location == "null" {
            return true;
        }
        if is_blocked_system_mount(location) {
            tracing::info!("Removing invalid Lima mount (system path): {}", location);
            return false;
        }
        if !Path::new(location).exists() {
            tracing::info!(
                "Removing stale Lima mount (path does not exist): {}",
                location
            );
            return false;
        }
        true
    });
    if mounts_seq.len() != original_len {
        modified = true;
    }

    // Phase 2: Add new mounts (skip blocked system paths).
    let existing: Vec<String> = mounts_seq
        .iter()
        .filter_map(|entry| {
            entry
                .get("location")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();

    for path in paths {
        if is_blocked_system_mount(path) {
            tracing::info!(
                "Skipping Lima mount for system path: {} (blocked by Lima)",
                path
            );
            continue;
        }
        if !existing.iter().any(|loc| loc == *path) {
            let mut mount_entry = serde_yaml::Mapping::new();
            mount_entry.insert(
                serde_yaml::Value::String("location".to_string()),
                serde_yaml::Value::String(path.to_string()),
            );
            mount_entry.insert(
                serde_yaml::Value::String("writable".to_string()),
                serde_yaml::Value::Bool(true),
            );
            mounts_seq.push(serde_yaml::Value::Mapping(mount_entry));
            tracing::info!("Adding Lima mount: {}", path);
            modified = true;
        }
    }

    if modified {
        let yaml_str = serde_yaml::to_string(&config).map_err(|e| {
            Error::LimaInstanceError(format!("failed to serialize Lima config: {e}"))
        })?;
        std::fs::write(config_path, yaml_str).map_err(|e| {
            Error::LimaInstanceError(format!(
                "failed to write Lima config {}: {e}",
                config_path.display()
            ))
        })?;
    }

    Ok(modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::fs;
    use tempfile::tempdir;

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

    #[test]
    fn test_ensure_extra_mounts_adds_new_mount() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "mounts:\n  - location: \"~\"\n    writable: true\n";
        fs::write(&config_path, initial).expect("write initial config");

        let modified =
            ensure_extra_mounts(&config_path, &["/tmp/test_mount"]).expect("should succeed");
        assert!(modified, "config should have been modified");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            content.contains("/tmp/test_mount"),
            "config should contain new mount path, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_skips_existing() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");

        // Use a real directory so the cleanup phase doesn't remove it as stale.
        let existing_dir = dir.path().join("existing_mount");
        fs::create_dir_all(&existing_dir).expect("create existing mount dir");
        let existing_path = existing_dir.to_str().unwrap();

        let initial = format!(
            "mounts:\n  - location: \"~\"\n    writable: true\n  - location: {existing_path}\n    writable: true\n"
        );
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[existing_path]).expect("should succeed");
        assert!(!modified, "config should not have been modified");
    }

    #[test]
    fn test_ensure_extra_mounts_creates_mounts_section() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "images: []\n";
        fs::write(&config_path, initial).expect("write initial config");

        let modified =
            ensure_extra_mounts(&config_path, &["/data/shared"]).expect("should succeed");
        assert!(modified, "config should have been modified");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            content.contains("/data/shared"),
            "config should contain new mount, got:\n{content}"
        );
    }

    // These tests previously spawned fake `limactl` shell scripts via tempdirs
    // and hit Linux ETXTBSY races: when Thread A held a writable FD to a newly
    // written script, a parallel test on Thread B calling `Command::spawn`
    // would fork, inheriting Thread A's writable FD, and the kernel refused
    // Thread A's subsequent `execve` until Thread B's child finished its own
    // `execve`. The fix is to exercise the pure branch logic directly via the
    // `validate_lima_version_output` and `stop_instance_inner` helpers — no
    // subprocess, no race.

    #[cfg(unix)]
    #[test]
    fn test_check_lima_version_returns_version_check_error_on_nonzero_exit() {
        use std::os::unix::process::ExitStatusExt;

        let status = std::process::ExitStatus::from_raw(42 << 8);
        let err = validate_lima_version_output(
            std::path::Path::new("/fake/limactl"),
            status,
            b"",
            b"bad lima",
        )
        .expect_err("expected version check failure");
        match err {
            Error::LimaVersionCheckFailed(msg) => {
                assert!(msg.contains("--version"), "unexpected message: {msg}");
                assert!(msg.contains("42"), "unexpected message: {msg}");
                assert!(msg.contains("bad lima"), "unexpected message: {msg}");
            }
            other => panic!("expected LimaVersionCheckFailed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_check_lima_version_rejects_unparseable_stdout() {
        // `parse_lima_version` returns `None` for garbage, which `unwrap_or_default()`
        // turns into (0, 0, 0) — that should fail the MIN_LIMA_VERSION check.
        let status = std::process::ExitStatus::default();
        let err = validate_lima_version_output(
            std::path::Path::new("/fake/limactl"),
            status,
            b"not a version",
            b"",
        )
        .expect_err("expected version-too-old failure");
        match err {
            Error::LimaVersionTooOld { found, minimum } => {
                assert_eq!(found, "0.0.0");
                assert_eq!(
                    minimum,
                    format!(
                        "{}.{}.{}",
                        MIN_LIMA_VERSION.0, MIN_LIMA_VERSION.1, MIN_LIMA_VERSION.2
                    )
                );
            }
            other => panic!("expected LimaVersionTooOld, got {other:?}"),
        }
    }

    #[test]
    fn test_stop_instance_is_idempotent_for_nonexistent() {
        let result = stop_instance_inner(
            "nonexistent_instance",
            || Ok(None),
            || panic!("stop should not be called for a non-existent instance"),
        );
        assert!(
            result.is_ok(),
            "stop_instance should succeed for non-existent instance, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_stop_instance_is_idempotent_for_stopped() {
        let result = stop_instance_inner(
            "stopped_instance",
            || Ok(Some("Stopped".to_string())),
            || panic!("stop should not be called for an already-stopped instance"),
        );
        assert!(
            result.is_ok(),
            "stop_instance should succeed for already-stopped instance, got: {:?}",
            result.unwrap_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_runs_stop_when_running() {
        use std::cell::Cell;

        let stop_called = Cell::new(false);
        let result = stop_instance_inner(
            "running_instance",
            || Ok(Some("Running".to_string())),
            || {
                stop_called.set(true);
                Ok(std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            },
        );
        assert!(
            result.is_ok(),
            "stop_instance should succeed when stop command succeeds, got: {:?}",
            result.unwrap_err()
        );
        assert!(stop_called.get(), "stop closure should have been invoked");
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_instance_reports_error_when_stop_fails() {
        use std::os::unix::process::ExitStatusExt;

        let result = stop_instance_inner(
            "running_instance",
            || Ok(Some("Running".to_string())),
            || {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: b"limactl exploded".to_vec(),
                })
            },
        );
        match result {
            Err(Error::LimaInstanceError(msg)) => {
                assert!(
                    msg.contains("running_instance"),
                    "unexpected message: {msg}"
                );
                assert!(
                    msg.contains("limactl exploded"),
                    "unexpected message: {msg}"
                );
            }
            other => panic!("expected LimaInstanceError, got {other:?}"),
        }
    }

    #[test]
    fn test_is_blocked_system_mount_rejects_top_level() {
        assert!(is_blocked_system_mount("/"));
        assert!(is_blocked_system_mount("/tmp"));
        assert!(is_blocked_system_mount("/var"));
        assert!(is_blocked_system_mount("/etc"));
        assert!(is_blocked_system_mount("/bin"));
        assert!(is_blocked_system_mount("/dev"));
        assert!(is_blocked_system_mount("/home"));
        assert!(is_blocked_system_mount("/opt"));
        assert!(is_blocked_system_mount("/sbin"));
        assert!(is_blocked_system_mount("/usr"));
    }

    #[test]
    fn test_is_blocked_system_mount_rejects_private_equivalents() {
        assert!(is_blocked_system_mount("/private/tmp"));
        assert!(is_blocked_system_mount("/private/var"));
        assert!(is_blocked_system_mount("/private/etc"));
    }

    #[test]
    fn test_is_blocked_system_mount_allows_subdirectories() {
        assert!(!is_blocked_system_mount("/tmp/my_app"));
        assert!(!is_blocked_system_mount("/var/log/my_app"));
        assert!(!is_blocked_system_mount("/data/shared"));
        assert!(!is_blocked_system_mount("/mnt/external"));
        assert!(!is_blocked_system_mount("/private/tmp/foo"));
    }

    #[test]
    fn test_ensure_extra_mounts_removes_system_path_mounts() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = r#"mounts:
- location: "~"
  writable: true
- location: /tmp
  writable: true
- location: /private/tmp
  writable: true
"#;
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[]).expect("should succeed");
        assert!(
            modified,
            "config should be modified (system path mounts removed)"
        );

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            !content.contains("location: /tmp"),
            "system path /tmp should be removed, got:\n{content}"
        );
        assert!(
            !content.contains("/private/tmp"),
            "system path /private/tmp should be removed, got:\n{content}"
        );
        assert!(
            content.contains("~"),
            "home mount should be preserved, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_removes_stale_mounts() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = r#"mounts:
- location: "~"
  writable: true
- location: /nonexistent/stale/path/from/old/test
  writable: true
"#;
        fs::write(&config_path, initial).expect("write initial config");

        let modified = ensure_extra_mounts(&config_path, &[]).expect("should succeed");
        assert!(modified, "config should be modified (stale mount removed)");

        let content = fs::read_to_string(&config_path).expect("read config");
        assert!(
            !content.contains("/nonexistent/stale/path"),
            "stale mount should be removed, got:\n{content}"
        );
        assert!(
            content.contains("~"),
            "home mount should be preserved, got:\n{content}"
        );
    }

    #[test]
    fn test_ensure_extra_mounts_skips_adding_system_paths() {
        let dir = tempdir().expect("create temp dir");
        let config_path = dir.path().join("lima.yaml");
        let initial = "mounts:\n- location: \"~\"\n  writable: true\n";
        fs::write(&config_path, initial).expect("write initial config");

        // Create a real directory so the valid path actually exists
        let valid_dir = dir.path().join("valid_mount");
        fs::create_dir_all(&valid_dir).expect("create valid mount dir");
        let valid_path = valid_dir.to_str().unwrap();

        let modified =
            ensure_extra_mounts(&config_path, &["/tmp", valid_path]).expect("should succeed");
        assert!(modified, "config should be modified (valid path added)");

        let content = fs::read_to_string(&config_path).expect("read config");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&content).expect("parse result config");
        let has_tmp_mount = parsed["mounts"]
            .as_sequence()
            .unwrap()
            .iter()
            .any(|m| m["location"].as_str() == Some("/tmp"));
        assert!(
            !has_tmp_mount,
            "system path /tmp should not be added, got:\n{content}"
        );
        assert!(
            content.contains(valid_path),
            "valid path should be added, got:\n{content}"
        );
    }
}
