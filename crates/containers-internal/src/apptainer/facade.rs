use super::super::error::{Error, Result};
use super::lima;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Serializes Lima VM initialization to prevent concurrent boot/sync races.
///
/// Multiple `Apptainer::new()` calls (e.g. from parallel test threads) would
/// otherwise race on `limactl start` and the guest apptainer tar sync,
/// corrupting the guest installation.
static LIMA_INIT: Mutex<()> = Mutex::new(());

/// Lima hostname that resolves to the macOS host IP from inside the guest VM.
const LIMA_HOST_GATEWAY: &str = "host.lima.internal";

/// Returns `true` if the string looks like a URI reference (e.g. `docker://...`, `library://...`)
/// rather than a filesystem path.
pub(crate) fn is_uri(s: &str) -> bool {
    s.contains("://")
}

/// The execution backend: how apptainer commands are actually invoked.
#[derive(Debug)]
pub(crate) enum Backend {
    /// Linux: run apptainer directly on the host.
    Native { apptainer_bin: PathBuf },
    /// macOS: route commands through a Lima VM.
    Lima {
        /// Path to `bin/apptainer` used for invocation (guest-side inside the VM).
        apptainer_bin: PathBuf,
        /// Path to the bundled `limactl` binary.
        limactl_path: PathBuf,
        /// LIMA_HOME directory for VM instance data.
        lima_home: PathBuf,
    },
}

/// Handle for the Apptainer container runtime.
///
/// Apptainer is installed as a portable, relocatable directory tree (created by
/// `install.sh`) rather than a single binary. This type resolves
/// the installation directory and provides command-builder methods for common
/// apptainer operations.
///
/// On macOS, commands are transparently routed through a bundled Lima VM since
/// apptainer is Linux-only. The host-side installation is synced to
/// `/tmp/peppy/apptainer/` inside the guest and all commands use the guest-side
/// path.
///
/// Construction validates paths, resolves the Lima installation (on macOS), and
/// ensures the backend is fully ready before returning. On Linux this completes
/// instantly; on macOS it boots the Lima VM and syncs apptainer into the guest
/// (may take minutes on first run).
#[derive(Debug)]
pub struct Apptainer {
    /// Root of the apptainer installation on the host (contains `bin/`, arch dirs, etc.)
    pub(crate) apptainer_dir: PathBuf,
    /// Execution backend (Native on Linux, Lima on macOS).
    pub(crate) backend: Backend,
    /// Host paths registered via `ensure_host_mounts()` that are accessible
    /// in the Lima VM even though they are outside `$HOME`.
    pub(crate) extra_mounts: Vec<PathBuf>,
}

/// Check if a command is available in PATH.
#[cfg(target_os = "linux")]
fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(cmd);
            if full.is_file() { Some(full) } else { None }
        })
    })
}

/// Check whether AppArmor profiles can actually be managed on this system.
///
/// Returns `true` when the AppArmor security filesystem is fully mounted,
/// meaning we can inspect loaded profiles and load new ones via
/// `apparmor_parser`.
///
/// We check for `policy/` inside the AppArmor directory because:
/// - On a real host with AppArmor, `/sys/kernel/security/apparmor/policy/`
///   exists and contains loaded profile entries.
/// - Inside containers (Docker, Podman, BuildKit, etc.), the directory is
///   either absent entirely or is an empty mount point with no `policy/`
///   subdirectory.
///
/// The procfs value `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`
/// is inherited from the host kernel and may read "1" even inside containers
/// where AppArmor cannot actually be managed.
#[cfg(target_os = "linux")]
fn is_apparmor_manageable() -> bool {
    Path::new("/sys/kernel/security/apparmor/policy").is_dir()
}

/// Status of Apptainer user namespace prerequisites on the current system.
///
/// Apptainer is built without setuid (`--without-suid`) and relies on
/// unprivileged user namespaces via fakeroot. This requires `newuidmap`
/// (from the `uidmap` package). On systems where AppArmor restricts
/// unprivileged user namespaces (e.g. Ubuntu 24.04+), an AppArmor profile
/// must be installed to allow the `starter` binary to create them.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct SetupStatus {
    /// `newuidmap` is available in PATH (required for fakeroot).
    pub newuidmap_ok: bool,
    /// System restricts unprivileged user namespaces via AppArmor.
    pub apparmor_restricted: bool,
    /// AppArmor profile for `starter` is installed and references the current
    /// binary path (always `true` when `apparmor_restricted` is `false`,
    /// or when AppArmor is not manageable).
    pub apparmor_ok: bool,
    /// AppArmor profile is loaded into the kernel (always `true` when
    /// `apparmor_restricted` is `false`, or when AppArmor is not manageable).
    pub apparmor_loaded: bool,
    /// Whether AppArmor profiles can be managed on this system.
    /// `false` when the AppArmor security filesystem is not mounted
    /// (e.g. inside Docker/Podman/BuildKit containers).
    pub apparmor_manageable: bool,
    /// A shell script that fixes all failing checks, or `None` when everything
    /// passes.
    pub fix_script: Option<String>,
}

#[cfg(target_os = "linux")]
impl SetupStatus {
    /// Returns `true` when all prerequisites are met.
    pub fn is_ok(&self) -> bool {
        self.newuidmap_ok && self.apparmor_ok && self.apparmor_loaded
    }
}

/// AppArmor profile identity for one apptainer installation.
///
/// The profile name embeds a stable hash of the canonical starter path, so
/// every installation on a machine (the version-keyed build caches under
/// `~/.peppy/tmp/`, installed release layouts, `PEPPY_APPTAINER_DIR`
/// overrides) gets its own profile file under `/etc/apparmor.d/`. A single
/// shared profile can only reference one starter path, so regenerating it for
/// one installation used to silently invalidate every other one: after an
/// apptainer version bump renamed the build cache, CI runs on commits from
/// either side of the bump needed different profiles and kept breaking each
/// other depending on which one ran `peppy container setup` last.
#[cfg(target_os = "linux")]
pub(crate) struct ApparmorProfileRef {
    /// Canonical starter path, exactly as embedded in the profile body.
    pub(crate) starter_path: String,
    /// AppArmor profile name (`peppy-apptainer-<hash>`).
    pub(crate) name: String,
    /// Profile file under `/etc/apparmor.d/`.
    pub(crate) file: PathBuf,
}

/// Resolve the AppArmor profile identity for the installation at `apptainer_dir`.
///
/// Uses the canonicalized starter path when the binary exists (the kernel
/// resolves symlinks at exec time, so the profile must reference the resolved
/// path) and falls back to the literal path otherwise.
#[cfg(target_os = "linux")]
pub(crate) fn apparmor_profile_ref(apptainer_dir: &Path) -> ApparmorProfileRef {
    let starter = apptainer_dir.join("libexec/apptainer/bin/starter");
    let starter_canonical = starter.canonicalize().unwrap_or(starter);
    let starter_path = starter_canonical.display().to_string();
    let name = format!("peppy-apptainer-{:016x}", fnv1a_64(starter_path.as_bytes()));
    let file = Path::new("/etc/apparmor.d").join(&name);
    ApparmorProfileRef {
        starter_path,
        name,
        file,
    }
}

/// 64-bit FNV-1a hash. Profile names derived from it are persisted in
/// `/etc/apparmor.d/`, so the hash must stay stable across peppy releases:
/// `DefaultHasher` gives no such guarantee, and a cryptographic hash would add
/// a dependency for no benefit (the hash only namespaces profile files, it is
/// not a security boundary).
#[cfg(target_os = "linux")]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

/// Escape a value for interpolation inside a single-quoted shell string: each
/// embedded quote terminates the string, inserts an escaped quote, and reopens
/// it, so the surrounding quoting survives arbitrary path characters. The fix
/// script embeds the starter path inside `echo '...'`; a home directory
/// containing a quote must not break out of it.
#[cfg(target_os = "linux")]
pub(crate) fn shell_escape_single_quoted(value: &str) -> String {
    value.replace('\'', r"'\''")
}

/// Inspect the Apptainer user namespace prerequisites without failing on errors.
///
/// Returns a [`SetupStatus`] describing which checks pass and which do not,
/// along with a ready-to-run fix script when something needs attention.
///
/// The caller is responsible for resolving the `apptainer_dir`; use
/// [`Apptainer::resolve_apptainer_dir`] or the `PEPPY_APPTAINER_DIR` env var.
#[cfg(target_os = "linux")]
pub fn check_setup_status(apptainer_dir: &Path) -> SetupStatus {
    let apparmor_manageable = is_apparmor_manageable();

    // newuidmap is required for fakeroot mode (unprivileged user namespaces).
    // It's provided by the `uidmap` package on Debian/Ubuntu, `shadow-utils`
    // on Fedora, and `shadow` on Arch Linux.
    let newuidmap_ok = which("newuidmap").is_some();

    // The procfs flag may read "1" even inside containers (inherited from
    // the host kernel). Only treat it as restricted when AppArmor is also
    // manageable; otherwise we'd try to load profiles into a kernel we
    // have no access to.
    let apparmor_restricted = apparmor_manageable
        && std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

    let profile = apparmor_profile_ref(apptainer_dir);

    let apparmor_ok = if apparmor_restricted {
        // The per-install profile file must exist AND reference the current
        // starter path. The hashed file name already namespaces installations;
        // the content check additionally catches a manually edited or
        // corrupted profile that would not grant the binary namespace
        // privileges.
        std::fs::read_to_string(&profile.file)
            .map(|content| content.contains(&profile.starter_path))
            .unwrap_or(false)
    } else {
        true
    };

    let apparmor_loaded = if apparmor_restricted {
        // /sys/kernel/security/apparmor/profiles requires CAP_MAC_ADMIN, so
        // use the policy directory listing which is world-readable.
        let kernel_entry_prefix = format!("{}.", profile.name);
        std::fs::read_dir("/sys/kernel/security/apparmor/policy/profiles")
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with(&kernel_entry_prefix))
                })
            })
            .unwrap_or(false)
    } else {
        true
    };

    let fix_script = if newuidmap_ok && apparmor_ok && apparmor_loaded {
        None
    } else {
        let mut parts: Vec<String> = Vec::new();

        if !newuidmap_ok {
            parts.push(
                "sudo apt-get install -y uidmap 2>/dev/null \
                 || sudo dnf install -y shadow-utils 2>/dev/null \
                 || sudo pacman -Sy --noconfirm shadow 2>/dev/null"
                    .to_string(),
            );
        }

        if apparmor_restricted && !apparmor_ok {
            parts.push(format!(
                "echo 'abi <abi/4.0>,\n\
                 include <tunables/global>\n\
                 \n\
                 profile {profile_name} {starter_path} flags=(unconfined) {{\n\
                 \x20 userns,\n\
                 }}' | sudo tee {profile_file} > /dev/null \\\n  \
                 && sudo apparmor_parser -r {profile_file}",
                profile_name = profile.name,
                starter_path = shell_escape_single_quoted(&profile.starter_path),
                profile_file = profile.file.display(),
            ));
        } else if apparmor_restricted && !apparmor_loaded {
            parts.push(format!(
                "sudo apparmor_parser -r {}",
                profile.file.display()
            ));
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" \\\n  && "))
        }
    };

    SetupStatus {
        newuidmap_ok,
        apparmor_restricted,
        apparmor_ok,
        apparmor_loaded,
        apparmor_manageable,
        fix_script,
    }
}

/// Check that Apptainer's user namespace prerequisites are met.
/// Returns an error with a copy-pasteable fix script when any check fails.
#[cfg(target_os = "linux")]
fn check_userns_prerequisites(apptainer_dir: &Path) -> Result<()> {
    let status = check_setup_status(apptainer_dir);
    if status.is_ok() {
        return Ok(());
    }

    let script = status
        .fix_script
        .unwrap_or_else(|| "peppy container setup".to_string());
    let indented: String = script.lines().fold(String::new(), |mut buf, line| {
        buf.push_str("  ");
        buf.push_str(line);
        buf.push('\n');
        buf
    });
    Err(Error::ConfigurationError(format!(
        "Apptainer's user namespace prerequisites are not met.\n\
         \n\
         To fix, run the following command:\n\
         \n\
         {indented}\n\
         Or run `peppy container setup` to fix this automatically."
    )))
}

impl Apptainer {
    /// Returns `true` if the Lima VM backend is already running and reachable.
    ///
    /// On Linux this always returns `true` (no VM needed). On macOS it checks
    /// whether the Lima instance is booted and SSH-reachable without starting it.
    pub fn is_lima_ready() -> bool {
        if !cfg!(target_os = "macos") {
            return true;
        }
        let Ok(lima_dir) = lima::resolve_lima_dir() else {
            return false;
        };
        let limactl_path = lima_dir.join("bin/limactl");
        let Ok(lima_home) = lima::resolve_lima_home() else {
            return false;
        };
        lima::is_lima_instance_running(&limactl_path, &lima_home)
    }

    /// Creates a new `Apptainer` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    ///
    /// # Blocking
    ///
    /// Construction runs [`ensure_ready`](Self::ensure_ready), so it is not free.
    /// On Linux it only checks user namespace prerequisites. On macOS it boots the
    /// Lima VM and syncs the apptainer install into the guest, which can take
    /// minutes on first run, so call it from a blocking context (e.g.
    /// `tokio::task::spawn_blocking`). Use [`is_lima_ready`](Self::is_lima_ready)
    /// to preflight without triggering a boot.
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        Self::from_dir(apptainer_dir)
    }

    /// Creates a new `Apptainer` from an explicit installation directory.
    ///
    /// Crate-internal: production callers use [`new`](Self::new); this is the
    /// explicit-dir seam it delegates to (and the entry point the construction
    /// tests drive). Blocks during construction the same way `new` does.
    pub(crate) fn from_dir(apptainer_dir: PathBuf) -> Result<Self> {
        let apptainer_bin = apptainer_dir.join("bin/apptainer");

        if !apptainer_bin.exists() {
            return Err(Error::ApptainerNotFound(format!(
                "bin/apptainer not found in installation directory: {}",
                apptainer_dir.display()
            )));
        }

        let backend = if cfg!(target_os = "macos") {
            let lima_dir = lima::resolve_lima_dir()?;
            let limactl_path = lima_dir.join("bin/limactl");
            if !limactl_path.exists() {
                return Err(Error::LimaRequired);
            }
            let lima_home = lima::resolve_lima_home()?;
            lima::check_lima_version(&limactl_path)?;

            Backend::Lima {
                apptainer_bin: PathBuf::from(env!("GUEST_APPTAINER_DIR")).join("bin/apptainer"),
                limactl_path,
                lima_home,
            }
        } else {
            Backend::Native { apptainer_bin }
        };

        let mut facade = Self {
            apptainer_dir,
            backend,
            extra_mounts: Vec::new(),
        };
        facade.ensure_ready()?;
        Ok(facade)
    }

    /// Ensures the execution backend is fully ready for running commands.
    /// Called once during construction.
    ///
    /// On Linux (`Backend::Native`): verifies user namespace prerequisites (AppArmor).
    ///
    /// On macOS (`Backend::Lima`): boots the Lima VM if it is not already running,
    /// and syncs the apptainer installation into the guest. This may take minutes
    /// on first run.
    fn ensure_ready(&mut self) -> Result<()> {
        match &mut self.backend {
            Backend::Native { .. } => {
                #[cfg(target_os = "linux")]
                check_userns_prerequisites(&self.apptainer_dir)?;
                Ok(())
            }
            Backend::Lima {
                limactl_path,
                lima_home,
                apptainer_bin,
                ..
            } => {
                let _guard = LIMA_INIT.lock().unwrap_or_else(|e| e.into_inner());

                lima::ensure_lima_instance(limactl_path, lima_home, lima::LIMA_TEMPLATE)?;
                lima::ensure_guest_userns(limactl_path, lima_home, lima::LIMA_INSTANCE)?;
                lima::ensure_guest_uidmap(limactl_path, lima_home, lima::LIMA_INSTANCE)?;

                *apptainer_bin = lima::ensure_guest_apptainer(
                    &self.apptainer_dir,
                    limactl_path,
                    lima_home,
                    lima::LIMA_INSTANCE,
                )?;

                Ok(())
            }
        }
    }

    /// Returns the hostname that resolves to the host machine from inside
    /// the execution environment.
    ///
    /// - `Backend::Lima`: `Some("host.lima.internal")`, Lima's built-in
    ///   hostname for guest-to-host connectivity.
    /// - `Backend::Native`: `None`; Apptainer shares the host network
    ///   namespace, so `127.0.0.1` already refers to the host.
    pub fn host_gateway(&self) -> Option<&'static str> {
        match &self.backend {
            Backend::Native { .. } => None,
            Backend::Lima { .. } => Some(LIMA_HOST_GATEWAY),
        }
    }

    /// Ensure that the given host paths are accessible inside the execution
    /// environment.
    ///
    /// On Linux (`Backend::Native`): no-op; all host paths are directly
    /// accessible.
    ///
    /// On macOS (`Backend::Lima`): Lima only auto-mounts `$HOME` into the
    /// guest VM. Paths outside `$HOME` must be explicitly added to the Lima
    /// configuration. This method updates the Lima YAML config with any
    /// missing mounts and restarts the VM if changes were made.
    pub fn ensure_host_mounts(&mut self, mount_src_paths: &[&str]) -> Result<()> {
        match &self.backend {
            Backend::Native { .. } => Ok(()),
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => {
                let home = std::env::var("HOME").map_err(|_| {
                    Error::ConfigurationError("HOME environment variable not set".into())
                })?;

                let external_paths: Vec<&str> = mount_src_paths
                    .iter()
                    .filter(|p| !resolve_absolute(p).starts_with(&home))
                    .copied()
                    .collect();

                if external_paths.is_empty() {
                    return Ok(());
                }

                let _guard = LIMA_INIT.lock().unwrap_or_else(|e| e.into_inner());

                let limactl_path = limactl_path.clone();
                let lima_home = lima_home.clone();
                let config_path = lima_home.join(lima::LIMA_INSTANCE).join("lima.yaml");

                let modified = lima::ensure_extra_mounts(&config_path, &external_paths)?;
                if modified {
                    tracing::info!("Lima config updated with new mounts, restarting VM...");
                    lima::stop_instance(&limactl_path, &lima_home, lima::LIMA_INSTANCE)?;
                    lima::ensure_lima_instance(&limactl_path, &lima_home, lima::LIMA_TEMPLATE)?;
                    lima::ensure_guest_userns(&limactl_path, &lima_home, lima::LIMA_INSTANCE)?;
                    let apptainer_bin = lima::ensure_guest_apptainer(
                        &self.apptainer_dir,
                        &limactl_path,
                        &lima_home,
                        lima::LIMA_INSTANCE,
                    )?;
                    if let Backend::Lima {
                        apptainer_bin: ref mut bin,
                        ..
                    } = self.backend
                    {
                        *bin = apptainer_bin;
                    }
                }

                // Register paths so translate_path() accepts them.
                for path_str in &external_paths {
                    let abs = resolve_absolute(path_str);
                    if !self.extra_mounts.contains(&abs) {
                        self.extra_mounts.push(abs);
                    }
                }

                Ok(())
            }
        }
    }

    pub fn version(&self) -> Result<String> {
        let mut cmd = self.command(&["--version"], &[], None, None)?;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().map_err(Error::from)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(Error::CommandFailed {
                command: "apptainer --version".to_string(),
                status: output.status,
                stderr,
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// SIGKILL the guest-side process group recorded for `key` by the Lima
    /// wrapper (see [`ApptainerCommand::cancel_pgid`]). `key` must match the one
    /// passed to `cancel_pgid` for this command; both resolve to the same
    /// guest-native pgid path via [`lima::guest_pgid_path`]. Used to cancel an
    /// in-flight build (`--force` supersede) and to force-stop a run node's
    /// in-VM workload on daemon teardown.
    ///
    /// On the native backend (Linux) the host process-group SIGKILL already
    /// reached the whole tree (shared namespace), so this is a no-op. Under Lima
    /// (macOS) the guest `apptainer` process and its children (a build's `%post`
    /// steps, a run's container workload) live in a separate kernel; this reaches
    /// into the VM and kills the whole group. Best-effort: a missing or
    /// already-dead group is not an error. A `limactl`-level failure (for example
    /// the VM being unreachable) is surfaced as an error, since the guest-side
    /// kill itself always exits zero (`lima_kill_pgid_argv` swallows a missing or
    /// already-dead group), so a non-zero exit means the invocation, not the
    /// group, failed.
    pub fn kill_guest_process_group(&self, key: &str) -> Result<()> {
        match &self.backend {
            Backend::Native { .. } => Ok(()),
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => Self::kill_guest_pgid(limactl_path, lima_home, key),
        }
    }

    /// Issues the in-VM group kill for `key` via `limactl shell`. Shared by
    /// [`kill_guest_process_group`](Self::kill_guest_process_group) (facade
    /// instance) and
    /// [`kill_guest_process_groups_best_effort`](Self::kill_guest_process_groups_best_effort)
    /// (no facade).
    ///
    /// Bounded: `limactl shell` reaches into the VM over SSH, which can hang
    /// on a wedged VM, and this runs on stop/teardown paths that must not
    /// block. On deadline expiry the `limactl` child is killed and a
    /// timeout-specific error returned.
    fn kill_guest_pgid(limactl_path: &Path, lima_home: &Path, key: &str) -> Result<()> {
        /// Generous upper bound for one in-VM `kill` round trip over `limactl shell`.
        const KILL_GUEST_PGID_TIMEOUT: Duration = Duration::from_secs(10);
        /// Poll cadence while waiting for the `limactl` child to exit.
        const KILL_GUEST_PGID_POLL: Duration = Duration::from_millis(50);

        let guest_pgid = lima::guest_pgid_path(key);
        let mut child = lima::lima_shell_cmd(limactl_path, lima_home, lima::LIMA_INSTANCE)
            .args(lima::lima_kill_pgid_argv(&guest_pgid))
            .spawn()
            .map_err(Error::from)?;
        await_guest_kill(
            &mut child,
            &guest_pgid,
            KILL_GUEST_PGID_TIMEOUT,
            KILL_GUEST_PGID_POLL,
            Instant::now,
            std::thread::sleep,
        )
    }

    /// Issues a cooperative in-VM SIGTERM for `key` via `limactl shell`. The
    /// guest pgid file is left in place so the force phase can still SIGKILL the
    /// same group if it ignores SIGTERM.
    ///
    /// Bounded for the same reason as [`kill_guest_pgid`](Self::kill_guest_pgid):
    /// stop/teardown paths cannot block indefinitely on a wedged VM or SSH
    /// channel.
    fn terminate_guest_pgid(limactl_path: &Path, lima_home: &Path, key: &str) -> Result<()> {
        /// Generous upper bound for one in-VM `kill` round trip over `limactl shell`.
        const TERMINATE_GUEST_PGID_TIMEOUT: Duration = Duration::from_secs(10);
        /// Poll cadence while waiting for the `limactl` child to exit.
        const TERMINATE_GUEST_PGID_POLL: Duration = Duration::from_millis(50);

        let guest_pgid = lima::guest_pgid_path(key);
        let mut child = lima::lima_shell_cmd(limactl_path, lima_home, lima::LIMA_INSTANCE)
            .args(lima::lima_terminate_pgid_argv(&guest_pgid))
            .spawn()
            .map_err(Error::from)?;
        await_guest_kill(
            &mut child,
            &guest_pgid,
            TERMINATE_GUEST_PGID_TIMEOUT,
            TERMINATE_GUEST_PGID_POLL,
            Instant::now,
            std::thread::sleep,
        )
    }

    /// Best-effort batch form of
    /// [`kill_guest_process_group`](Self::kill_guest_process_group) for node
    /// stop / daemon teardown. Owns the platform gate so callers need no
    /// `cfg!(target_os = "macos")` checks: a no-op on non-macOS hosts (the host
    /// process-group SIGKILL already reached the shared-namespace workload) and
    /// when the Lima VM is not running (its guest processes died with it;
    /// never boot a VM just to kill processes inside it, which a full
    /// [`Apptainer::new`] readiness preflight could do). Failures are logged at
    /// debug level, never returned: a guest-kill problem must not block a stop.
    ///
    /// Synchronous (it shells out to `limactl`), so call it from a blocking
    /// context (e.g. `tokio::task::spawn_blocking`).
    pub fn kill_guest_process_groups_best_effort(keys: &[String]) {
        if !cfg!(target_os = "macos") || keys.is_empty() {
            return;
        }
        let (limactl_path, lima_home) = match (lima::resolve_lima_dir(), lima::resolve_lima_home())
        {
            (Ok(lima_dir), Ok(lima_home)) => (lima_dir.join("bin/limactl"), lima_home),
            // No resolvable Lima installation means no VM, hence no guest
            // processes to kill.
            _ => return,
        };
        if !lima::is_lima_instance_running(&limactl_path, &lima_home) {
            return;
        }
        for key in keys {
            if let Err(e) = Self::kill_guest_pgid(&limactl_path, &lima_home, key) {
                tracing::debug!("In-VM guest group kill failed for '{key}': {e}");
            }
        }
    }

    /// Best-effort batch form of the guest-side cooperative SIGTERM used by
    /// node stop / daemon teardown before the force-kill phase. Owns the same
    /// platform gate as
    /// [`kill_guest_process_groups_best_effort`](Self::kill_guest_process_groups_best_effort):
    /// a no-op outside macOS/Lima, when no keys are provided, or when the VM is
    /// not running. Failures are logged at debug level, never returned; a failed
    /// cooperative signal still falls through to the existing SIGKILL phase.
    /// Returns `true` when the Lima guest-signal path was available and attempted
    /// for the provided keys; callers can use `false` to fall back to host-side
    /// signaling on native Apptainer.
    ///
    /// Synchronous (it shells out to `limactl`), so call it from a blocking
    /// context (e.g. `tokio::task::spawn_blocking`).
    pub fn terminate_guest_process_groups_best_effort(keys: &[String]) -> bool {
        if !cfg!(target_os = "macos") || keys.is_empty() {
            return false;
        }
        let (limactl_path, lima_home) = match (lima::resolve_lima_dir(), lima::resolve_lima_home())
        {
            (Ok(lima_dir), Ok(lima_home)) => (lima_dir.join("bin/limactl"), lima_home),
            // No resolvable Lima installation means no VM, hence no guest
            // processes to signal.
            _ => return false,
        };
        if !lima::is_lima_instance_running(&limactl_path, &lima_home) {
            return false;
        }
        for key in keys {
            if let Err(e) = Self::terminate_guest_pgid(&limactl_path, &lima_home, key) {
                tracing::debug!("In-VM guest group SIGTERM failed for '{key}': {e}");
            }
        }
        true
    }

    /// Run `args` in the container runtime environment and capture its output.
    ///
    /// On Linux (`Backend::Native`) the command runs directly on the host. On
    /// macOS (`Backend::Lima`) it runs inside the guest VM via
    /// `limactl shell <instance> -- <args>`. Complements
    /// [`kill_guest_process_group`](Self::kill_guest_process_group) for diagnostics
    /// and lifecycle checks that need to observe runtime-side processes (the same
    /// kernel the build/run executes in).
    pub fn guest_command(&self, args: &[&str]) -> Result<std::process::Output> {
        let (program, rest) = args.split_first().ok_or_else(|| {
            Error::ConfigurationError("guest_command requires at least one argument".into())
        })?;
        match &self.backend {
            Backend::Native { .. } => Command::new(program)
                .args(rest)
                .output()
                .map_err(Error::from),
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => lima::lima_shell_cmd(limactl_path, lima_home, lima::LIMA_INSTANCE)
                .args(args)
                .output()
                .map_err(Error::from),
        }
    }

    // -----------------------------------------------------------------------
    // Command builders
    // -----------------------------------------------------------------------

    /// Start building an `apptainer run` command.
    ///
    /// Returns a builder that can be configured with flags (e.g. `--bind`,
    /// `--env`) before being executed with [`.spawn()`](ApptainerCommand::spawn)
    /// or [`.output()`](ApptainerCommand::output).
    ///
    /// If `image` is a filesystem path (not a URI like `docker://...`) it is
    /// translated to a guest-visible path when running under Lima.
    ///
    /// # Example
    /// ```no_run
    /// # let facade = containers::Apptainer::new()?;
    /// let mut child = facade.run("image.sif")
    ///     .bind("/dev/ttyUSB0", None, None)
    ///     .env("ROS_DOMAIN_ID", "42")
    ///     .spawn()?;
    /// # Ok::<(), containers::Error>(())
    /// ```
    pub fn run(&self, image: &str) -> ApptainerCommand<'_> {
        ApptainerCommand {
            facade: self,
            kind: CommandKind::Run {
                image: image.to_string(),
                args: Vec::new(),
            },
            flags: Vec::new(),
            bind_mounts: Vec::new(),
            lima_shell_extra_args: Vec::new(),
            cancel_pgid_path: None,
            working_dir: None,
        }
    }

    /// Start building an `apptainer exec` command.
    ///
    /// If `container` is a filesystem path (not a URI) it is translated to a
    /// guest-visible path when running under Lima.
    pub fn exec(&self, container: &str, cmd: &[&str]) -> ApptainerCommand<'_> {
        ApptainerCommand {
            facade: self,
            kind: CommandKind::Exec {
                container: container.to_string(),
                cmd: cmd.iter().map(|s| s.to_string()).collect(),
            },
            flags: Vec::new(),
            bind_mounts: Vec::new(),
            lima_shell_extra_args: Vec::new(),
            cancel_pgid_path: None,
            working_dir: None,
        }
    }

    /// Start building an `apptainer build` command.
    ///
    /// Both paths are translated to guest-visible paths when running under Lima.
    pub fn build(&self, output: &Path, def_file: &Path) -> ApptainerCommand<'_> {
        ApptainerCommand {
            facade: self,
            kind: CommandKind::Build {
                output: output.to_string_lossy().into_owned(),
                def_file: def_file.to_string_lossy().into_owned(),
            },
            flags: Vec::new(),
            bind_mounts: Vec::new(),
            lima_shell_extra_args: Vec::new(),
            cancel_pgid_path: None,
            working_dir: None,
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Translate a string argument: if it is a URI, return as-is; otherwise
    /// resolve as a filesystem path via [`translate_path()`](Self::translate_path).
    fn translate_arg(&self, arg: &str) -> Result<String> {
        if is_uri(arg) {
            Ok(arg.to_string())
        } else {
            Ok(self
                .translate_path(Path::new(arg))?
                .to_string_lossy()
                .into_owned())
        }
    }

    /// Translate a host-side path to its guest-visible equivalent.
    ///
    /// When running natively (Linux), all paths are returned unchanged (but
    /// relative paths are resolved to absolute).
    ///
    /// When running under Lima (macOS), Lima auto-mounts the home directory (`~`)
    /// at the same absolute path inside the guest. Paths under `$HOME` are returned
    /// unchanged. Paths outside `$HOME` are accepted if they were registered via
    /// [`ensure_host_mounts()`](Self::ensure_host_mounts); otherwise an error is
    /// returned.
    pub(crate) fn translate_path(&self, host_path: &Path) -> Result<PathBuf> {
        // Resolve relative paths to absolute using the host CWD. This is critical
        // for Lima: `limactl shell` runs in the guest's home directory, so a
        // relative path would silently resolve to the wrong location in the guest.
        let absolute_path = to_absolute(host_path)?;

        match &self.backend {
            Backend::Native { .. } => Ok(absolute_path),
            Backend::Lima { .. } => {
                let home = std::env::var("HOME").map_err(|_| {
                    Error::ConfigurationError("HOME environment variable not set".into())
                })?;

                if absolute_path.starts_with(&home) {
                    return Ok(absolute_path);
                }

                // Check paths registered via ensure_host_mounts().
                if self
                    .extra_mounts
                    .iter()
                    .any(|m| absolute_path.starts_with(m))
                {
                    return Ok(absolute_path);
                }

                Err(Error::PathNotAccessibleInVm {
                    path: absolute_path.display().to_string(),
                })
            }
        }
    }

    /// Build a [`Command`] that will invoke apptainer with the given arguments.
    ///
    /// On Linux: runs `{apptainer_bin} <args...>` directly, with `working_dir`
    /// (when set) applied via [`Command::current_dir`].
    /// On macOS: runs `{limactl} shell peppy -- {guest_apptainer_bin} <args...>` to
    /// execute inside the Lima VM using the synced guest-side binary. A
    /// `working_dir` is translated via [`translate_path`](Self::translate_path)
    /// and entered by an explicit, loudly-failing `cd` inside the guest wrapper.
    /// It must NOT be applied as the host command's `current_dir`: `limactl
    /// shell` propagates the host cwd into the guest with a NON-fatal `cd`, and
    /// the kernel reports the cwd in canonical form (`/private/var/…`) while the
    /// Lima mount exists in the guest only at its literal lima.yaml location
    /// (`/var/folders/…`) — so the propagation silently falls back to the guest
    /// home directory and relative paths (a def file's `%files .`) resolve
    /// against `$HOME` instead of the working dir.
    ///
    /// `guest_pgid_file` (Lima only) wraps the guest invocation so apptainer runs
    /// as a process-group leader recording its PGID there; see
    /// [`lima::lima_guest_pgid_argv`]. The native backend ignores it (the host
    /// process group already covers the whole tree in the shared namespace).
    fn command(
        &self,
        args: &[&str],
        lima_shell_extra_args: &[String],
        guest_pgid_file: Option<&Path>,
        working_dir: Option<&Path>,
    ) -> Result<Command> {
        match &self.backend {
            Backend::Native { apptainer_bin } => {
                let mut cmd = Command::new(apptainer_bin);
                cmd.args(args);
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                Ok(cmd)
            }
            Backend::Lima {
                apptainer_bin,
                limactl_path,
                lima_home,
                ..
            } => {
                let guest_workdir = working_dir
                    .map(|dir| self.translate_path(dir))
                    .transpose()?;
                let mut cmd = lima::lima_shell_base(limactl_path, lima_home, lima::LIMA_INSTANCE);
                for arg in lima_shell_extra_args {
                    cmd.arg(arg);
                }
                cmd.arg("--");
                match (guest_pgid_file, guest_workdir.as_deref()) {
                    (Some(pgid_file), workdir) => {
                        cmd.args(lima::lima_guest_pgid_argv(
                            apptainer_bin,
                            args,
                            pgid_file,
                            workdir,
                        ));
                    }
                    (None, Some(workdir)) => {
                        cmd.args(lima::lima_guest_workdir_argv(apptainer_bin, args, workdir));
                    }
                    (None, None) => {
                        cmd.arg(apptainer_bin);
                        cmd.args(args);
                    }
                }
                Ok(cmd)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Path resolution
    // -----------------------------------------------------------------------

    pub fn resolve_apptainer_dir() -> Result<PathBuf> {
        lima::resolve_install_dir(
            "PEPPY_APPTAINER_DIR",
            "apptainer",
            option_env!("APPTAINER_INSTALL_DIR"),
            "APPTAINER_INSTALL_DIR",
            || {
                Error::ApptainerNotFound(
                    "Apptainer installation not found. Install apptainer or set PEPPY_APPTAINER_DIR."
                        .to_string(),
                )
            },
        )
    }
}

/// Resolve a relative path to absolute by joining it onto the current working
/// directory; absolute paths are returned unchanged. Shared by [`translate_path`]
/// (which propagates a `current_dir()` failure) and [`resolve_absolute`] (which
/// falls back to the original path), so both normalize identically. That shared
/// normalization is load-bearing: `ensure_host_mounts` registers extra mounts via
/// `resolve_absolute` and `translate_path` later matches against them with
/// `starts_with`, so a divergence here would silently break mount matching.
fn to_absolute(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_relative() {
        Ok(std::env::current_dir()?.join(path))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Resolve a potentially relative path to an absolute one.
///
/// Falls back to the original path if `current_dir()` fails.
fn resolve_absolute(path: &str) -> PathBuf {
    to_absolute(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path))
}

/// The subset of [`std::process::Child`] that [`await_guest_kill`] needs, behind a
/// trait so the bounded-wait decision logic can be unit-tested with a fake child
/// instead of a real `limactl` subprocess (mirroring the injected-closure pattern
/// of [`lima::stop_instance_inner`]).
pub(crate) trait GuestKillChild {
    /// Non-blocking check for the child's exit status.
    fn poll_exit(&mut self) -> Result<Option<ExitStatus>>;
    /// Kill the child and reap it (best effort), used when the wait times out.
    fn kill_and_reap(&mut self);
}

impl GuestKillChild for Child {
    fn poll_exit(&mut self) -> Result<Option<ExitStatus>> {
        self.try_wait().map_err(Error::from)
    }

    fn kill_and_reap(&mut self) {
        let _ = self.kill();
        let _ = self.wait();
    }
}

/// Wait for `child` to exit, bounded by `timeout`, polling every `poll_interval`.
///
/// Returns `Ok(Some(status))` when the child exits before the deadline, or
/// `Ok(None)` when the deadline elapses first (the child is killed and reaped
/// before returning). `clock` and `sleep` are injected so the decision logic is
/// unit-tested on a virtual clock with no real sleeping (production passes
/// `Instant::now` and `thread::sleep`). Shared by [`await_guest_kill`] and the
/// bounded VM-liveness probes in [`super::lima`], so every `limactl` invocation
/// on a stop/teardown path carries the same kill-on-deadline guarantee.
pub(crate) fn wait_for_child_bounded(
    child: &mut impl GuestKillChild,
    timeout: Duration,
    poll_interval: Duration,
    mut clock: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> Result<Option<ExitStatus>> {
    let deadline = clock() + timeout;
    loop {
        if let Some(status) = child.poll_exit()? {
            return Ok(Some(status));
        }
        if clock() >= deadline {
            child.kill_and_reap();
            return Ok(None);
        }
        sleep(poll_interval);
    }
}

/// Wait for the guest-kill `limactl` child to exit, bounded by `timeout`.
///
/// A clean exit returns `Ok(())`; a non-zero exit returns the limactl-failure
/// error; a timeout kills and reaps the child and returns the timeout error. The
/// bounded-wait mechanics live in [`wait_for_child_bounded`]; this only maps its
/// outcome to the guest-kill result.
pub(crate) fn await_guest_kill(
    child: &mut impl GuestKillChild,
    guest_pgid: &Path,
    timeout: Duration,
    poll_interval: Duration,
    clock: impl FnMut() -> Instant,
    sleep: impl FnMut(Duration),
) -> Result<()> {
    match wait_for_child_bounded(child, timeout, poll_interval, clock, sleep)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(Error::LimaInstanceError(format!(
            "failed to kill guest process group (pgid file {}): limactl exited with {}",
            guest_pgid.display(),
            status
        ))),
        None => Err(Error::LimaInstanceError(format!(
            "timed out after {timeout:?} killing guest process group (pgid file {})",
            guest_pgid.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// ApptainerCommand builder
// ---------------------------------------------------------------------------

/// Bind mount specification (path translation deferred to spawn/output).
struct BindMount {
    src: String,
    dest: Option<String>,
    opts: Option<String>,
}

/// The kind of apptainer subcommand being built.
enum CommandKind {
    /// `apptainer run <image> [container-args...]`
    Run { image: String, args: Vec<String> },
    /// `apptainer exec <container> <command> [cmd-args...]`
    Exec { container: String, cmd: Vec<String> },
    /// `apptainer build <output> <def_file>`
    Build { output: String, def_file: String },
}

/// Builder for an apptainer command with optional flags.
///
/// Created via [`Apptainer::run()`], [`Apptainer::exec()`],
/// or [`Apptainer::build()`]. Flags are accumulated with chained
/// method calls, then the command is executed with [`.spawn()`](Self::spawn)
/// or [`.output()`](Self::output).
///
/// All flag methods return `Self` for clean chaining. Path translation errors
/// (e.g. bind mount paths outside `$HOME` on macOS) are deferred to the terminal
/// methods (`.spawn()` / `.output()`).
///
/// # Example
/// ```no_run
/// # let facade = containers::Apptainer::new()?;
/// // Mount multiple devices and set environment variables
/// let mut child = facade.run("image.sif")
///     .bind("/dev/ttyUSB0", None, None)
///     .bind("/dev/can0", None, None)
///     .env("ROS_DOMAIN_ID", "42")
///     .spawn()?;
///
/// // Variable number of devices at runtime
/// let devices = vec!["/dev/ttyUSB0", "/dev/can0"];
/// let mut cmd = facade.run("image.sif");
/// for dev in &devices {
///     cmd = cmd.bind(dev, None, None);
/// }
/// let mut child = cmd.spawn()?;
/// # Ok::<(), containers::Error>(())
/// ```
pub struct ApptainerCommand<'a> {
    facade: &'a Apptainer,
    kind: CommandKind,
    flags: Vec<String>,
    bind_mounts: Vec<BindMount>,
    lima_shell_extra_args: Vec<String>,
    /// When set, run the guest command (Lima only) as a process-group leader that
    /// records its PGID to this guest-native path, so
    /// [`Apptainer::kill_guest_process_group`] can SIGKILL the whole guest group on
    /// cancel/teardown. Resolved from a key via [`lima::guest_pgid_path`].
    /// Meaningful for `build` (cancel an in-flight `--force` supersede) and `run`
    /// (force-stop the in-VM workload on daemon teardown).
    cancel_pgid_path: Option<PathBuf>,
    /// Host-side working directory for the apptainer process. See
    /// [`ApptainerCommand::working_dir`].
    working_dir: Option<PathBuf>,
}

impl<'a> ApptainerCommand<'a> {
    // -----------------------------------------------------------------------
    // Named flag methods
    // -----------------------------------------------------------------------

    /// Add a `--bind src[:dest[:opts]]` mount.
    ///
    /// The host-side `src` path is automatically translated for Lima on macOS
    /// when the command is executed. If `dest` is `None`, the container-side
    /// path mirrors the host path. Optional `opts` (e.g. `"ro"`, `"rw"`) are
    /// appended after the destination.
    pub fn bind(mut self, src: &str, dest: Option<&str>, opts: Option<&str>) -> Self {
        self.bind_mounts.push(BindMount {
            src: src.to_string(),
            dest: dest.map(|d| d.to_string()),
            opts: opts.map(|o| o.to_string()),
        });
        self
    }

    /// Add a `--env VAR=VALUE` environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.flags.push("--env".to_string());
        self.flags.push(format!("{key}={value}"));
        self
    }

    /// Run with `--cleanenv`: the container does NOT inherit the host (daemon)
    /// process environment. Only the explicit `--env` vars set on this command
    /// and the image's own `%environment` are visible inside.
    ///
    /// Two reasons a spawned node wants this. First, hygiene and security: the
    /// daemon's environment can hold secrets (ssh-agent sockets, tokens) and
    /// host-specific noise that have no business inside a node. Second,
    /// robustness: without it, apptainer folds the inherited environment into a
    /// generated `/.inject-apptainer-env.sh` that the container sources at
    /// startup, and a single host var whose name is not a shell identifier (for
    /// example a bash exported function `BASH_FUNC_x%%`) makes that `source`
    /// abort with "invalid var name", silently dropping every later var,
    /// including `PEPPY_RUNTIME_CONFIG`. The node then falls back to its
    /// standalone defaults instead of the daemon-provided parameters. Passing
    /// the node's environment explicitly via `--env` (which still applies under
    /// `--cleanenv`) keeps the curated set while removing both hazards.
    pub fn clean_env(mut self) -> Self {
        self.flags.push("--cleanenv".to_string());
        self
    }

    /// Add extra arguments passed to `limactl shell` (before the `--` separator).
    ///
    /// These are only effective when running under the Lima backend (macOS).
    /// On Linux (native backend) they are silently ignored.
    pub fn lima_shell_extra_args(mut self, args: &[String]) -> Self {
        self.lima_shell_extra_args.extend(args.iter().cloned());
        self
    }

    /// Make the guest command a process-group leader that records its PGID to a
    /// guest-native file keyed by `key`, so [`Apptainer::kill_guest_process_group`]
    /// (called with the same key) can SIGKILL the whole guest group (apptainer and
    /// its children) on cancellation/teardown. `key` must be unique and
    /// filesystem-safe: the working-dir basename for a build, the instance id for
    /// a run.
    ///
    /// Only effective under the Lima backend (macOS), and used by `build` (cancel
    /// an in-flight `--force` supersede) and `run` (force-stop the in-VM workload
    /// on daemon teardown). On the native backend (Linux) the host process-group
    /// SIGKILL already covers the whole tree in the shared namespace, so this is
    /// ignored.
    pub fn cancel_pgid(mut self, key: &str) -> Self {
        self.cancel_pgid_path = Some(lima::guest_pgid_path(key));
        self
    }

    /// Run the apptainer process from `dir` (a host-side path), so relative
    /// paths — chiefly a build def's `%files` sources like the generated
    /// `%files . /opt/{name}` — resolve against it.
    ///
    /// On the native backend (Linux) this is applied as the child's
    /// `current_dir`. Under Lima (macOS) the path is translated to its
    /// guest-visible form and entered by an explicit `cd` inside the guest
    /// wrapper that ABORTS the command if the directory is not accessible.
    /// Callers must use this instead of setting `current_dir` on the returned
    /// [`Command`]: `limactl shell`'s own host-cwd propagation canonicalizes
    /// the path (`/var/folders/…` → `/private/var/…`), misses the literal
    /// Lima mount location in the guest, and falls back to the guest home
    /// directory WITHOUT failing — which once made a build's `%files .` copy
    /// the user's entire `$HOME` into the image.
    pub fn working_dir(mut self, dir: &Path) -> Self {
        self.working_dir = Some(dir.to_path_buf());
        self
    }

    // -----------------------------------------------------------------------
    // Generic flag / args
    // -----------------------------------------------------------------------

    /// Add a raw flag not covered by the named methods.
    ///
    /// For flags that take a value, call this twice:
    /// `.raw_flag("--overlay").raw_flag("/path/to/overlay")`.
    pub fn raw_flag(mut self, flag: &str) -> Self {
        self.flags.push(flag.to_string());
        self
    }

    /// Append arguments after the positional args.
    ///
    /// For `run`: these become container arguments passed to the runscript.
    /// For `exec`: these are appended to the command.
    pub fn args(mut self, args: &[&str]) -> Self {
        match &mut self.kind {
            CommandKind::Run { args: a, .. } => {
                a.extend(args.iter().map(|s| s.to_string()));
            }
            CommandKind::Exec { cmd: c, .. } => {
                c.extend(args.iter().map(|s| s.to_string()));
            }
            CommandKind::Build { .. } => {
                debug_assert!(false, "args() is not applicable to build commands");
            }
        }
        self
    }

    // -----------------------------------------------------------------------
    // Terminal methods
    // -----------------------------------------------------------------------

    /// Spawn the command, returning a handle to the child process.
    ///
    /// Stdout and stderr are inherited (not piped), so build/run progress
    /// output flows directly to the terminal.
    pub fn spawn(self) -> Result<Child> {
        let mut cmd = self.assemble_command()?;
        cmd.spawn().map_err(Error::from)
    }

    /// Assemble the fully-configured [`Command`] (subcommand + flags + binds +
    /// positional args, wrapped in `limactl shell` under Lima) shared by the
    /// terminal methods. The cancel-PGID path is already guest-native, so it is
    /// passed straight through to wrap the Lima build as a process-group leader.
    fn assemble_command(&self) -> Result<Command> {
        let args = self.build_args()?;
        let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        // The pgid path comes from `lima::guest_pgid_path`, i.e. it is already a
        // guest-side path, so it must NOT go through `translate_path` (which
        // would reject `/tmp/...` as outside `$HOME`).
        self.facade.command(
            &str_args,
            &self.lima_shell_extra_args,
            self.cancel_pgid_path.as_deref(),
            self.working_dir.as_deref(),
        )
    }

    /// Build the fully-configured [`Command`] without spawning it.
    ///
    /// This is useful when callers need to customize stdio piping (e.g., for
    /// async output capture via `tokio::process::Command`) or add additional
    /// process-level configuration before spawning.
    ///
    /// The returned command has **no stdio overrides**: stdout, stderr, and
    /// stdin all default to `Inherit`.
    pub fn into_std_command(self) -> Result<Command> {
        self.assemble_command()
    }

    /// Run the command to completion and return its captured output.
    ///
    /// Stdout and stderr are piped (captured).
    pub fn output(self) -> Result<Output> {
        let mut cmd = self.assemble_command()?;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.output().map_err(Error::from)
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Assemble the full argument vector: `[subcommand, ...flags, ...binds, ...positional]`.
    ///
    /// Path translation for bind mounts and positional args happens here.
    pub(crate) fn build_args(&self) -> Result<Vec<String>> {
        let mut args = Vec::new();

        // 1. Subcommand
        args.push(
            match &self.kind {
                CommandKind::Run { .. } => "run",
                CommandKind::Exec { .. } => "exec",
                CommandKind::Build { .. } => "build",
            }
            .to_string(),
        );

        // 2. Accumulated flags (no translation needed)
        args.extend(self.flags.iter().cloned());

        // 3. Bind mounts (translate src paths for Lima)
        for bind in &self.bind_mounts {
            args.push("--bind".to_string());
            let translated_src = self.facade.translate_arg(&bind.src)?;
            match (&bind.dest, &bind.opts) {
                (Some(dest), Some(opts)) => args.push(format!("{translated_src}:{dest}:{opts}")),
                (Some(dest), None) => args.push(format!("{translated_src}:{dest}")),
                (None, _) => args.push(translated_src),
            }
        }

        // 4. Positional args (with path translation)
        match &self.kind {
            CommandKind::Run { image, args: extra } => {
                args.push(self.facade.translate_arg(image)?);
                args.extend(extra.iter().cloned());
            }
            CommandKind::Exec { container, cmd } => {
                args.push(self.facade.translate_arg(container)?);
                args.extend(cmd.iter().cloned());
            }
            CommandKind::Build { output, def_file } => {
                args.push(
                    self.facade
                        .translate_path(Path::new(output))?
                        .to_string_lossy()
                        .into_owned(),
                );
                args.push(
                    self.facade
                        .translate_path(Path::new(def_file))?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        Ok(args)
    }
}
