use super::super::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

const LIMA_INSTANCE: &str = "peppy";
const MIN_LIMA_VERSION: (u32, u32, u32) = (2, 0, 0);

/// Returns `true` if the string looks like a URI reference (e.g. `docker://...`, `library://...`)
/// rather than a filesystem path.
pub(crate) fn is_uri(s: &str) -> bool {
    s.contains("://")
}

/// Single-quote a path for safe embedding in a shell command string.
fn shell_escape(path: &Path) -> String {
    // Replace any single quotes in the path with the '\'' idiom, then wrap in single quotes.
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
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
#[derive(Debug)]
pub struct ApptainerFacade {
    /// Root of the apptainer installation on the host (contains `bin/`, arch dirs, etc.)
    pub(crate) apptainer_dir: PathBuf,
    /// Host-side path to `bin/apptainer` within the installation.
    pub(crate) apptainer_bin: PathBuf,
    /// Path to `bin/apptainer` as invoked in commands. On Linux this is the same as
    /// `apptainer_bin`. On macOS (Lima) this is the guest-side path at `/tmp/peppy/apptainer/`.
    pub(crate) guest_apptainer_bin: PathBuf,
    /// Whether to route commands through Lima (macOS).
    pub(crate) use_lima: bool,
    /// Path to the bundled `limactl` binary. `None` on Linux.
    pub(crate) limactl_path: Option<PathBuf>,
    /// LIMA_HOME directory for VM instance data. `None` on Linux.
    pub(crate) lima_home: Option<PathBuf>,
}

impl ApptainerFacade {
    /// Creates a new `ApptainerFacade` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `../apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    ///
    /// On macOS, also resolves the bundled Lima installation, ensures the Lima
    /// VM instance is running, and syncs apptainer into the guest.
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        Self::from_dir(apptainer_dir)
    }

    /// Creates a new `ApptainerFacade` from an explicit installation directory.
    ///
    /// Validates that `bin/apptainer` exists within `apptainer_dir`. On macOS,
    /// resolves the bundled Lima installation, ensures the VM instance is
    /// running, and syncs apptainer into the guest.
    pub fn from_dir(apptainer_dir: PathBuf) -> Result<Self> {
        let apptainer_bin = apptainer_dir.join("bin/apptainer");

        if !apptainer_bin.exists() {
            return Err(Error::ApptainerNotFound(format!(
                "bin/apptainer not found in installation directory: {}",
                apptainer_dir.display()
            )));
        }

        let use_lima = cfg!(target_os = "macos");

        let (limactl_path, lima_home) = if use_lima {
            let lima_dir = Self::resolve_lima_dir()?;
            let limactl = lima_dir.join("bin/limactl");
            if !limactl.exists() {
                return Err(Error::LimaRequired);
            }
            let home = Self::resolve_lima_home()?;
            Self::check_lima_version(&limactl)?;
            Self::ensure_lima_instance(&limactl, &home, "template:apptainer")?;
            (Some(limactl), Some(home))
        } else {
            (None, None)
        };

        let guest_apptainer_bin = if use_lima {
            Self::ensure_guest_apptainer(
                &apptainer_dir,
                limactl_path.as_ref().unwrap(),
                lima_home.as_ref().unwrap(),
                LIMA_INSTANCE,
            )?
        } else {
            apptainer_bin.clone()
        };

        Ok(Self {
            apptainer_dir,
            apptainer_bin,
            guest_apptainer_bin,
            use_lima,
            limactl_path,
            lima_home,
        })
    }

    /// Returns the root directory of the apptainer installation on the host.
    pub fn install_dir(&self) -> &Path {
        &self.apptainer_dir
    }

    /// Returns the host-side path to the apptainer binary.
    pub fn binary_path(&self) -> &Path {
        &self.apptainer_bin
    }

    /// Returns the path used to invoke apptainer in commands.
    ///
    /// On Linux this is the same as [`binary_path()`](Self::binary_path). On macOS
    /// (Lima) this is the guest-side path inside the VM.
    pub fn effective_binary_path(&self) -> &Path {
        &self.guest_apptainer_bin
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
        let translated = if is_uri(image) {
            image.to_string()
        } else {
            self.translate_path(Path::new(image))?
                .to_string_lossy()
                .into_owned()
        };
        let mut all_args = vec!["run", &translated];
        all_args.extend(args);
        self.spawn(&all_args)
    }

    /// Execute a command inside a container: `apptainer exec <container> <cmd...>`
    ///
    /// If `container` is a filesystem path (not a URI like `docker://...`) it is
    /// translated to a guest-visible path when running under Lima.
    pub fn exec(&self, container: &str, cmd: &[&str]) -> Result<Child> {
        let translated = if is_uri(container) {
            container.to_string()
        } else {
            self.translate_path(Path::new(container))?
                .to_string_lossy()
                .into_owned()
        };
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

    /// Translate a host-side path to its guest-visible equivalent.
    ///
    /// When `use_lima` is false (Linux), all paths are returned unchanged.
    ///
    /// When `use_lima` is true (macOS), Lima auto-mounts the home directory (`~`)
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

        if !self.use_lima {
            return Ok(absolute_path);
        }

        let home = std::env::var("HOME")
            .map_err(|_| Error::ConfigurationError("HOME environment variable not set".into()))?;

        if absolute_path.starts_with(&home) {
            Ok(absolute_path)
        } else {
            Err(Error::PathNotAccessibleInVm {
                path: absolute_path.display().to_string(),
            })
        }
    }

    /// Spawn an apptainer command with the given arguments.
    ///
    /// On macOS this routes through the bundled `limactl shell peppy --`.
    fn spawn(&self, args: &[&str]) -> Result<Child> {
        let mut cmd = self.command(args);
        cmd.spawn().map_err(Error::from)
    }

    /// Run an apptainer command to completion and return its output.
    fn run_to_completion(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.output().map_err(Error::from)
    }

    /// Build a [`Command`] that will invoke apptainer with the given arguments.
    ///
    /// On Linux: runs `{apptainer_bin} <args...>` directly.
    /// On macOS: runs `{limactl} shell peppy -- {guest_apptainer_bin} <args...>` to
    /// execute inside the Lima VM using the synced guest-side binary.
    fn command(&self, args: &[&str]) -> Command {
        if self.use_lima {
            let limactl = self
                .limactl_path
                .as_ref()
                .expect("limactl_path required on macOS");
            let lima_home = self
                .lima_home
                .as_ref()
                .expect("lima_home required on macOS");
            let mut cmd = Command::new(limactl);
            cmd.env("LIMA_HOME", lima_home);
            cmd.arg("shell").arg(LIMA_INSTANCE).arg("--");
            cmd.arg(&self.guest_apptainer_bin);
            cmd.args(args);
            cmd
        } else {
            let mut cmd = Command::new(&self.apptainer_bin);
            cmd.args(args);
            cmd
        }
    }

    // -----------------------------------------------------------------------
    // Lima instance and version management
    // -----------------------------------------------------------------------

    /// Check that the bundled Lima version meets the minimum requirement.
    fn check_lima_version(limactl: &Path) -> Result<()> {
        let output = Command::new(limactl).arg("--version").output()?;

        if !output.status.success() {
            return Err(Error::LimaRequired);
        }

        let version_str = String::from_utf8_lossy(&output.stdout);
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

    /// Ensure the peppy Lima instance exists and is running.
    ///
    /// * If the instance does not exist, create and start it with `template`.
    /// * If it exists but is stopped, start it.
    /// * If it is already running, this is a no-op.
    fn ensure_lima_instance(limactl: &Path, lima_home: &Path, template: &str) -> Result<()> {
        std::fs::create_dir_all(lima_home).map_err(|e| {
            Error::LimaInstanceError(format!(
                "failed to create LIMA_HOME {}: {e}",
                lima_home.display()
            ))
        })?;

        let list_output = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["list", "--format", "{{.Status}}", LIMA_INSTANCE])
            .output();

        let instance_status = match &list_output {
            Ok(o) if o.status.success() => {
                let status = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if status.is_empty() {
                    None
                } else {
                    Some(status)
                }
            }
            _ => None,
        };

        match instance_status.as_deref() {
            Some("Running") => Ok(()),
            Some(_) => {
                tracing::info!("Starting Lima {} instance...", LIMA_INSTANCE);
                let start = Command::new(limactl)
                    .env("LIMA_HOME", lima_home)
                    .args(["start", LIMA_INSTANCE])
                    .output()?;

                if start.status.success() {
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&start.stderr);
                    Err(Error::LimaInstanceError(format!(
                        "failed to start Lima {} instance: {stderr}",
                        LIMA_INSTANCE
                    )))
                }
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
                    .args(["start", &name_flag, "--tty=false", template])
                    .output()?;

                if create.status.success() {
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&create.stderr);
                    Err(Error::LimaInstanceError(format!(
                        "failed to create Lima {} instance: {stderr}",
                        LIMA_INSTANCE
                    )))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Guest apptainer sync
    // -----------------------------------------------------------------------

    /// Ensure the apptainer installation is available inside the Lima VM guest.
    ///
    /// Syncs the host-side installation to `/tmp/peppy/apptainer/` in the guest.
    /// This path lives on the guest's native writable filesystem, avoiding Lima's
    /// read-only home directory mount. A version-stamped marker file avoids
    /// redundant copies on subsequent invocations.
    ///
    /// Returns the guest-side path to `bin/apptainer`.
    fn ensure_guest_apptainer(
        host_dir: &Path,
        limactl: &Path,
        lima_home: &Path,
        instance: &str,
    ) -> Result<PathBuf> {
        let guest_dir = PathBuf::from("/tmp/peppy/apptainer");
        let guest_bin = guest_dir.join("bin/apptainer");

        let version = option_env!("APPTAINER_VERSION").unwrap_or("unknown");
        let marker_name = format!(".peppy-sync-{version}");

        // Fast path: check if the version marker exists (sub-second limactl call).
        let marker_exists = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["shell", instance, "--", "test", "-f"])
            .arg(guest_dir.join(&marker_name))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        if marker_exists {
            return Ok(guest_bin);
        }

        tracing::info!("Syncing apptainer installation to Lima VM guest...");

        // Remove stale installation in guest.
        let _ = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["shell", instance, "--", "rm", "-rf"])
            .arg(&guest_dir)
            .status();

        // Create the target directory in the guest.
        let mkdir = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["shell", instance, "--", "mkdir", "-p"])
            .arg(&guest_dir)
            .output()
            .map_err(|e| Error::LimaSyncFailed(format!("failed to create guest directory: {e}")))?;

        if !mkdir.status.success() {
            let stderr = String::from_utf8_lossy(&mkdir.stderr);
            return Err(Error::LimaSyncFailed(format!(
                "mkdir in guest returned {}: {stderr}",
                mkdir.status
            )));
        }

        // Copy host installation to guest via tar pipe.
        // `limactl copy -r` is unreliable with long or special-character paths,
        // so we tar on the host and untar in the guest through a pipe.
        let limactl_str = limactl.to_string_lossy();
        let lima_home_str = lima_home.to_string_lossy();
        let tar_pipe = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "tar -cf - -C {} . | LIMA_HOME={} {} shell {} -- tar -xf - -C {}",
                shell_escape(host_dir),
                shell_escape(Path::new(&*lima_home_str)),
                shell_escape(Path::new(&*limactl_str)),
                instance,
                guest_dir.display(),
            ))
            .output()
            .map_err(|e| Error::LimaSyncFailed(format!("tar pipe to guest failed: {e}")))?;

        if !tar_pipe.status.success() {
            let stderr = String::from_utf8_lossy(&tar_pipe.stderr);
            return Err(Error::LimaSyncFailed(format!(
                "tar pipe to guest returned {}: {stderr}",
                tar_pipe.status
            )));
        }

        // Write the version marker so we skip the sync next time.
        let _ = Command::new(limactl)
            .env("LIMA_HOME", lima_home)
            .args(["shell", instance, "--", "touch"])
            .arg(guest_dir.join(&marker_name))
            .status();

        Ok(guest_bin)
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

        // Platform-specific error
        #[cfg(target_os = "macos")]
        {
            Err(Error::LimaRequired)
        }

        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::ApptainerNotFound(
                "Apptainer installation not found. Install apptainer or set PEPPY_APPTAINER_DIR."
                    .to_string(),
            ))
        }
    }

    /// Resolve the Lima installation directory (contains `bin/limactl`, `share/lima/`).
    ///
    /// Resolution order:
    /// 1. `PEPPY_LIMA_DIR` environment variable
    /// 2. `../lima/` relative to the current executable (installed layout)
    /// 3. Compile-time `LIMA_INSTALL_DIR` set by build.rs
    fn resolve_lima_dir() -> Result<PathBuf> {
        // 1) Runtime override via environment variable
        if let Ok(dir) = std::env::var("PEPPY_LIMA_DIR") {
            let dir = dir.trim().to_string();
            if !dir.is_empty() {
                let path = PathBuf::from(&dir);
                if path.is_dir() {
                    return Ok(path);
                }
                tracing::warn!(
                    "PEPPY_LIMA_DIR={} does not exist or is not a directory",
                    dir
                );
            }
        }

        // 2) Relative to the current executable: {exe_dir}/../lima/
        //    This is the installed layout created by install.sh ($PEPPY_HOME/lima/).
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let candidate = exe_dir.join("../lima");
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }

        // 3) Compile-time path injected by build.rs
        if let Some(dir) = option_env!("LIMA_INSTALL_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                return Ok(path);
            }
            tracing::debug!(
                "Compile-time LIMA_INSTALL_DIR={} does not exist at runtime",
                dir
            );
        }

        Err(Error::LimaRequired)
    }

    /// Resolve the LIMA_HOME directory for VM instance data.
    ///
    /// Resolution order:
    /// 1. `{exe_dir}/../lima-data/` — installed layout (`~/.peppy/lima-data/`).
    /// 2. Compile-time `LIMA_BUILD_HOME` — reuses the build-time VM during development.
    /// 3. `~/.peppy/lima-data/` — fallback for unusual layouts.
    fn resolve_lima_home() -> Result<PathBuf> {
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
