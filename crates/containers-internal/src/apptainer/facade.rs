use super::super::error::{Error, Result};
use super::lima;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;

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

/// The execution backend — how apptainer commands are actually invoked.
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
/// Returns `true` when the AppArmor security filesystem is fully mounted
/// — meaning we can inspect loaded profiles and load new ones via
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

/// Inspect the Apptainer user namespace prerequisites without failing on errors.
///
/// Returns a [`SetupStatus`] describing which checks pass and which do not,
/// along with a ready-to-run fix script when something needs attention.
///
/// The caller is responsible for resolving the `apptainer_dir` — use
/// [`Apptainer::resolve_apptainer_dir`] or the `PEPPY_APPTAINER_DIR` env var.
#[cfg(target_os = "linux")]
pub fn check_setup_status(apptainer_dir: &Path) -> SetupStatus {
    let starter = apptainer_dir.join("libexec/apptainer/bin/starter");
    let apparmor_manageable = is_apparmor_manageable();

    // newuidmap is required for fakeroot mode (unprivileged user namespaces).
    // It's provided by the `uidmap` package on Debian/Ubuntu, `shadow-utils`
    // on Fedora, and `shadow` on Arch Linux.
    let newuidmap_ok = which("newuidmap").is_some();

    // The procfs flag may read "1" even inside containers (inherited from
    // the host kernel). Only treat it as restricted when AppArmor is also
    // manageable — otherwise we'd try to load profiles into a kernel we
    // have no access to.
    let apparmor_restricted = apparmor_manageable
        && std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

    let starter_canonical = starter.canonicalize().unwrap_or_else(|_| starter.clone());

    let apparmor_ok = if apparmor_restricted {
        // The profile must exist AND reference the current starter path.
        // A stale profile pointing to a different binary (e.g. a previous build
        // artifact) won't grant the current binary namespace privileges.
        std::fs::read_to_string("/etc/apparmor.d/peppy-apptainer")
            .map(|content| content.contains(&starter_canonical.display().to_string()))
            .unwrap_or(false)
    } else {
        true
    };

    let apparmor_loaded = if apparmor_restricted {
        // /sys/kernel/security/apparmor/profiles requires CAP_MAC_ADMIN, so
        // use the policy directory listing which is world-readable.
        std::fs::read_dir("/sys/kernel/security/apparmor/policy/profiles")
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("peppy-apptainer."))
                })
            })
            .unwrap_or(false)
    } else {
        true
    };

    let fix_script = if newuidmap_ok && apparmor_ok && apparmor_loaded {
        None
    } else {
        let starter_path = starter_canonical.display();
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
                 profile peppy-apptainer {starter_path} flags=(unconfined) {{\n\
                 \x20 userns,\n\
                 }}' | sudo tee /etc/apparmor.d/peppy-apptainer > /dev/null \\\n  \
                 && sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer"
            ));
        } else if apparmor_restricted && !apparmor_loaded {
            parts.push("sudo apparmor_parser -r /etc/apparmor.d/peppy-apptainer".to_string());
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
    /// Creates a new `Apptainer` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        Self::from_dir(apptainer_dir)
    }

    /// Creates a new `Apptainer` from an explicit installation directory.
    pub fn from_dir(apptainer_dir: PathBuf) -> Result<Self> {
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

    pub fn install_dir(&self) -> &Path {
        &self.apptainer_dir
    }

    /// Returns the hostname that resolves to the host machine from inside
    /// the execution environment.
    ///
    /// - `Backend::Lima`: `Some("host.lima.internal")` — Lima's built-in
    ///   hostname for guest-to-host connectivity.
    /// - `Backend::Native`: `None` — Apptainer shares the host network
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
    /// On Linux (`Backend::Native`): no-op — all host paths are directly
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

    /// Returns the path to the apptainer binary used for invocation.
    ///
    /// On Linux this is the host-side binary. On macOS (Lima) this is the
    /// guest-side path inside the VM.
    pub fn binary_path(&self) -> &Path {
        match &self.backend {
            Backend::Native { apptainer_bin } | Backend::Lima { apptainer_bin, .. } => {
                apptainer_bin
            }
        }
    }

    pub fn version(&self) -> Result<String> {
        let mut cmd = self.command(&["--version"], &[])?;
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
        let absolute_path = if host_path.is_relative() {
            std::env::current_dir()?.join(host_path)
        } else {
            host_path.to_path_buf()
        };

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
    /// On Linux: runs `{apptainer_bin} <args...>` directly.
    /// On macOS: runs `{limactl} shell peppy -- {guest_apptainer_bin} <args...>` to
    /// execute inside the Lima VM using the synced guest-side binary.
    fn command(&self, args: &[&str], lima_shell_extra_args: &[String]) -> Result<Command> {
        match &self.backend {
            Backend::Native { apptainer_bin } => {
                let mut cmd = Command::new(apptainer_bin);
                cmd.args(args);
                Ok(cmd)
            }
            Backend::Lima {
                apptainer_bin,
                limactl_path,
                lima_home,
                ..
            } => {
                let mut cmd = Command::new(limactl_path);
                cmd.env("LIMA_HOME", lima_home);
                cmd.arg("shell").arg(lima::LIMA_INSTANCE);
                for arg in lima_shell_extra_args {
                    cmd.arg(arg);
                }
                cmd.arg("--");
                cmd.arg(apptainer_bin);
                cmd.args(args);
                Ok(cmd)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Path resolution
    // -----------------------------------------------------------------------

    pub fn resolve_apptainer_dir() -> Result<PathBuf> {
        // 1) Runtime override via environment variable
        if let Ok(dir) = std::env::var("PEPPY_APPTAINER_DIR") {
            let dir = dir.trim().to_string();
            if !dir.is_empty() {
                let path = PathBuf::from(&dir);
                if path.is_dir() {
                    return Ok(path);
                }
                tracing::warn!(
                    "PEPPY_APPTAINER_DIR={} does not exist or is not a directory",
                    dir
                );
            }
        }

        // 2) Relative to the current executable: {exe_dir}/apptainer/
        //    This is the installed layout created by install.sh ($PEPPY_BIN_DIR/apptainer/).
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let candidate = exe_dir.join("apptainer");
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }

        // 3) Compile-time path injected by build.rs
        if let Some(dir) = option_env!("APPTAINER_INSTALL_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                return Ok(path);
            }
            tracing::debug!(
                "Compile-time APPTAINER_INSTALL_DIR={} does not exist at runtime",
                dir
            );
        }

        Err(Error::ApptainerNotFound(
            "Apptainer installation not found. Install apptainer or set PEPPY_APPTAINER_DIR."
                .to_string(),
        ))
    }
}

/// Resolve a potentially relative path to an absolute one.
///
/// Falls back to the original path if `current_dir()` fails.
fn resolve_absolute(path: &str) -> PathBuf {
    if Path::new(path).is_relative() {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| PathBuf::from(path))
    } else {
        PathBuf::from(path)
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

    /// Add multiple `--bind` mounts at once.
    ///
    /// Convenience method for mounting a variable number of devices at runtime.
    /// Each entry is bound with the same path inside the container.
    pub fn binds(mut self, sources: &[&str]) -> Self {
        for src in sources {
            self.bind_mounts.push(BindMount {
                src: src.to_string(),
                dest: None,
                opts: None,
            });
        }
        self
    }

    /// Add a `--env VAR=VALUE` environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.flags.push("--env".to_string());
        self.flags.push(format!("{key}={value}"));
        self
    }

    /// Add the `--writable-tmpfs` flag.
    pub fn writable_tmpfs(mut self) -> Self {
        self.flags.push("--writable-tmpfs".to_string());
        self
    }

    /// Add the `--no-home` flag (do not mount `$HOME` in the container).
    pub fn no_home(mut self) -> Self {
        self.flags.push("--no-home".to_string());
        self
    }

    /// Add the `--contain` flag (minimal `/dev` and empty home/tmp).
    pub fn contain(mut self) -> Self {
        self.flags.push("--contain".to_string());
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
        let args = self.build_args()?;
        let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = self
            .facade
            .command(&str_args, &self.lima_shell_extra_args)?;
        cmd.spawn().map_err(Error::from)
    }

    /// Build the fully-configured [`Command`] without spawning it.
    ///
    /// This is useful when callers need to customize stdio piping (e.g., for
    /// async output capture via `tokio::process::Command`) or add additional
    /// process-level configuration before spawning.
    ///
    /// The returned command has **no stdio overrides** — stdout, stderr, and
    /// stdin all default to `Inherit`.
    pub fn into_std_command(self) -> Result<Command> {
        let args = self.build_args()?;
        let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.facade.command(&str_args, &self.lima_shell_extra_args)
    }

    /// Run the command to completion and return its captured output.
    ///
    /// Stdout and stderr are piped (captured).
    pub fn output(self) -> Result<Output> {
        let args = self.build_args()?;
        let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = self
            .facade
            .command(&str_args, &self.lima_shell_extra_args)?;
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
