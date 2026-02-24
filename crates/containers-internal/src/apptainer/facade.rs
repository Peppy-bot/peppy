use super::super::error::{Error, Result};
use super::lima;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

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
        /// Host-side path to `bin/apptainer` within the installation (for validation).
        apptainer_bin: PathBuf,
        /// Guest-side path at `/tmp/peppy/apptainer/bin/apptainer`.
        guest_apptainer_bin: PathBuf,
        /// Path to the bundled `limactl` binary.
        limactl_path: PathBuf,
        /// LIMA_HOME directory for VM instance data.
        lima_home: PathBuf,
        /// Whether `ensure_ready()` has been called (VM booted, apptainer synced).
        ready: bool,
    },
}

/// Facade for the Apptainer container runtime.
///
/// Apptainer is installed as a portable, relocatable directory tree (created by
/// `install-unprivileged.sh`) rather than a single binary. The facade resolves
/// the installation directory and provides command-builder methods for common
/// apptainer operations.
///
/// On macOS, commands are transparently routed through a bundled Lima VM since
/// apptainer is Linux-only. The host-side installation is synced to
/// `/tmp/peppy/apptainer/` inside the guest and all commands use the guest-side
/// path.
///
/// # Two-phase initialization
///
/// Construction (`new()` / `from_dir()`) is cheap — it validates paths and
/// resolves the Lima installation but does **not** boot the VM. Call
/// [`ensure_ready()`](Self::ensure_ready) before running any commands. On Linux
/// this is a no-op; on macOS it boots the Lima VM and syncs apptainer into the
/// guest (may take minutes on first run).
#[derive(Debug)]
pub struct ApptainerFacade {
    /// Root of the apptainer installation on the host (contains `bin/`, arch dirs, etc.)
    pub(crate) apptainer_dir: PathBuf,
    /// Execution backend (Native on Linux, Lima on macOS).
    pub(crate) backend: Backend,
}

impl ApptainerFacade {
    /// Creates a new `ApptainerFacade` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `../apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    ///
    /// On macOS, also resolves the bundled Lima installation and validates the
    /// Lima version, but does **not** boot the VM. Call [`ensure_ready()`](Self::ensure_ready)
    /// to start the VM and sync apptainer into the guest.
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        Self::from_dir(apptainer_dir)
    }

    /// Creates a new `ApptainerFacade` from an explicit installation directory.
    ///
    /// Validates that `bin/apptainer` exists within `apptainer_dir`. On macOS,
    /// resolves the bundled Lima installation and checks the version, but does
    /// **not** boot the VM. Call [`ensure_ready()`](Self::ensure_ready) before
    /// running commands.
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
                apptainer_bin,
                guest_apptainer_bin: PathBuf::from("/tmp/peppy/apptainer/bin/apptainer"),
                limactl_path,
                lima_home,
                ready: false,
            }
        } else {
            Backend::Native { apptainer_bin }
        };

        Ok(Self {
            apptainer_dir,
            backend,
        })
    }

    /// Ensures the execution backend is fully ready for running commands.
    ///
    /// On Linux (`Backend::Native`): no-op, returns `Ok(())` immediately.
    ///
    /// On macOS (`Backend::Lima`): boots the Lima VM if it is not already running,
    /// and syncs the apptainer installation into the guest. This may take minutes
    /// on first run. Subsequent calls are idempotent.
    pub fn ensure_ready(&mut self) -> Result<()> {
        match &mut self.backend {
            Backend::Native { .. } => Ok(()),
            Backend::Lima {
                limactl_path,
                lima_home,
                guest_apptainer_bin,
                ready,
                ..
            } => {
                if *ready {
                    return Ok(());
                }

                lima::ensure_lima_instance(limactl_path, lima_home, lima::LIMA_TEMPLATE)?;

                *guest_apptainer_bin = lima::ensure_guest_apptainer(
                    &self.apptainer_dir,
                    limactl_path,
                    lima_home,
                    lima::LIMA_INSTANCE,
                )?;

                *ready = true;
                Ok(())
            }
        }
    }

    /// Returns the root directory of the apptainer installation on the host.
    pub fn install_dir(&self) -> &Path {
        &self.apptainer_dir
    }

    /// Returns the host-side path to the apptainer binary.
    pub fn binary_path(&self) -> &Path {
        match &self.backend {
            Backend::Native { apptainer_bin } | Backend::Lima { apptainer_bin, .. } => {
                apptainer_bin
            }
        }
    }

    /// Returns the path used to invoke apptainer in commands.
    ///
    /// On Linux this is the same as [`binary_path()`](Self::binary_path). On macOS
    /// (Lima) this is the guest-side path inside the VM.
    pub fn effective_binary_path(&self) -> &Path {
        match &self.backend {
            Backend::Native { apptainer_bin } => apptainer_bin,
            Backend::Lima {
                guest_apptainer_bin,
                ..
            } => guest_apptainer_bin,
        }
    }

    /// Query the apptainer version: `apptainer --version`
    ///
    /// Returns the version string (e.g. "apptainer version 1.4.5") on success.
    pub fn version(&self) -> Result<String> {
        let output = self.run_to_completion(&["--version"])?;
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

    /// Run a container image: `apptainer run <image> [args...]`
    ///
    /// If `image` is a filesystem path (not a URI like `docker://...`) it is
    /// translated to a guest-visible path when running under Lima.
    pub fn run(&self, image: &str, args: &[&str]) -> Result<Child> {
        let translated = self.translate_arg(image)?;
        let mut all_args = vec!["run", &translated];
        all_args.extend(args);
        self.spawn(&all_args)
    }

    /// Execute a command inside a container: `apptainer exec <container> <cmd...>`
    ///
    /// If `container` is a filesystem path (not a URI like `docker://...`) it is
    /// translated to a guest-visible path when running under Lima.
    pub fn exec(&self, container: &str, cmd: &[&str]) -> Result<Child> {
        let translated = self.translate_arg(container)?;
        let mut all_args = vec!["exec", &translated];
        all_args.extend(cmd);
        self.spawn(&all_args)
    }

    /// Build a container image: `apptainer build <output> <def_file>`
    ///
    /// Both paths are translated to guest-visible paths when running under Lima.
    pub fn build(&self, output: &Path, def_file: &Path) -> Result<Child> {
        let output_translated = self.translate_path(output)?;
        let def_translated = self.translate_path(def_file)?;
        let output_str = output_translated.to_string_lossy();
        let def_str = def_translated.to_string_lossy();
        self.spawn(&["build", &output_str, &def_str])
    }

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
    /// unchanged. Paths outside `$HOME` cannot be accessed by the guest and produce
    /// an error.
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
                    Ok(absolute_path)
                } else {
                    Err(Error::PathNotAccessibleInVm {
                        path: absolute_path.display().to_string(),
                    })
                }
            }
        }
    }

    /// Spawn an apptainer command with the given arguments.
    ///
    /// On macOS this routes through the bundled `limactl shell peppy --`.
    fn spawn(&self, args: &[&str]) -> Result<Child> {
        let mut cmd = self.command(args)?;
        cmd.spawn().map_err(Error::from)
    }

    /// Run an apptainer command to completion and return its output.
    fn run_to_completion(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = self.command(args)?;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.output().map_err(Error::from)
    }

    /// Build a [`Command`] that will invoke apptainer with the given arguments.
    ///
    /// On Linux: runs `{apptainer_bin} <args...>` directly.
    /// On macOS: runs `{limactl} shell peppy -- {guest_apptainer_bin} <args...>` to
    /// execute inside the Lima VM using the synced guest-side binary.
    ///
    /// Returns `Error::NotReady` if the Lima backend has not been initialized via
    /// [`ensure_ready()`](Self::ensure_ready).
    fn command(&self, args: &[&str]) -> Result<Command> {
        match &self.backend {
            Backend::Native { apptainer_bin } => {
                let mut cmd = Command::new(apptainer_bin);
                cmd.args(args);
                Ok(cmd)
            }
            Backend::Lima {
                guest_apptainer_bin,
                limactl_path,
                lima_home,
                ready,
                ..
            } => {
                if !ready {
                    return Err(Error::NotReady);
                }
                let mut cmd = Command::new(limactl_path);
                cmd.env("LIMA_HOME", lima_home);
                cmd.arg("shell").arg(lima::LIMA_INSTANCE).arg("--");
                cmd.arg(guest_apptainer_bin);
                cmd.args(args);
                Ok(cmd)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Path resolution
    // -----------------------------------------------------------------------

    fn resolve_apptainer_dir() -> Result<PathBuf> {
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

        // 2) Relative to the current executable: {exe_dir}/../apptainer/
        //    This is the installed layout created by install.sh ($PEPPY_HOME/apptainer/).
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let candidate = exe_dir.join("../apptainer");
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
