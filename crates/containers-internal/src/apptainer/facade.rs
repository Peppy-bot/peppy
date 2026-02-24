use super::super::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

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
/// On macOS, commands are transparently routed through a Lima VM since apptainer
/// is Linux-only. The host-side installation is synced to `/tmp/peppy/apptainer/`
/// inside the guest and all commands use the guest-side path.
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
}

impl ApptainerFacade {
    /// Creates a new `ApptainerFacade` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `../apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    ///
    /// On macOS, the host-side installation is synced into the Lima VM at
    /// `/tmp/peppy/apptainer/` so that commands can find the binary.
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        Self::from_dir(apptainer_dir)
    }

    /// Creates a new `ApptainerFacade` from an explicit installation directory.
    ///
    /// Validates that `bin/apptainer` exists within `apptainer_dir`. On macOS,
    /// syncs the installation into the Lima VM guest.
    pub fn from_dir(apptainer_dir: PathBuf) -> Result<Self> {
        let apptainer_bin = apptainer_dir.join("bin/apptainer");

        if !apptainer_bin.exists() {
            return Err(Error::ApptainerNotFound(format!(
                "bin/apptainer not found in installation directory: {}",
                apptainer_dir.display()
            )));
        }

        let use_lima = cfg!(target_os = "macos");

        let guest_apptainer_bin = if use_lima {
            Self::ensure_guest_apptainer(&apptainer_dir)?
        } else {
            apptainer_bin.clone()
        };

        Ok(Self {
            apptainer_dir,
            apptainer_bin,
            guest_apptainer_bin,
            use_lima,
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
    /// If `image` is an absolute path or exists on disk it is translated to a
    /// guest-visible path when running under Lima.
    pub fn run(&self, image: &str, args: &[&str]) -> Result<Child> {
        let image_path = Path::new(image);
        let translated = if image_path.is_absolute() {
            self.translate_path(image_path)?
                .to_string_lossy()
                .into_owned()
        } else {
            image.to_string()
        };
        let mut all_args = vec!["run", &translated];
        all_args.extend(args);
        self.spawn(&all_args)
    }

    /// Execute a command inside a container: `apptainer exec <container> <cmd...>`
    ///
    /// If `container` is an absolute path it is translated to a guest-visible path
    /// when running under Lima.
    pub fn exec(&self, container: &str, cmd: &[&str]) -> Result<Child> {
        let container_path = Path::new(container);
        let translated = if container_path.is_absolute() {
            self.translate_path(container_path)?
                .to_string_lossy()
                .into_owned()
        } else {
            container.to_string()
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
        if !self.use_lima {
            return Ok(host_path.to_path_buf());
        }

        let home = std::env::var("HOME")
            .map_err(|_| Error::ConfigurationError("HOME environment variable not set".into()))?;

        if host_path.starts_with(&home) {
            Ok(host_path.to_path_buf())
        } else {
            Err(Error::PathNotAccessibleInVm {
                path: host_path.display().to_string(),
            })
        }
    }

    /// Spawn an apptainer command with the given arguments.
    ///
    /// On macOS this routes through `limactl shell default --`.
    fn spawn(&self, args: &[&str]) -> Result<Child> {
        let mut cmd = self.command(args);
        cmd.spawn().map_err(Error::from)
    }

    /// Run an apptainer command to completion and return its output.
    fn run_to_completion(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = self.command(args);
        cmd.output().map_err(Error::from)
    }

    /// Build a [`Command`] that will invoke apptainer with the given arguments.
    ///
    /// On Linux: runs `{apptainer_bin} <args...>` directly.
    /// On macOS: runs `limactl shell default -- {guest_apptainer_bin} <args...>` to
    /// execute inside the Lima VM using the synced guest-side binary.
    fn command(&self, args: &[&str]) -> Command {
        if self.use_lima {
            let mut cmd = Command::new("limactl");
            cmd.arg("shell").arg("default").arg("--");
            cmd.arg(&self.guest_apptainer_bin);
            cmd.args(args);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        } else {
            let mut cmd = Command::new(&self.apptainer_bin);
            cmd.args(args);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        }
    }

    /// Ensure the apptainer installation is available inside the Lima VM guest.
    ///
    /// Syncs the host-side installation to `/tmp/peppy/apptainer/` in the guest.
    /// This path lives on the guest's native writable filesystem, avoiding Lima's
    /// read-only home directory mount. A version-stamped marker file avoids
    /// redundant copies on subsequent invocations.
    ///
    /// Returns the guest-side path to `bin/apptainer`.
    fn ensure_guest_apptainer(host_dir: &Path) -> Result<PathBuf> {
        let guest_dir = PathBuf::from("/tmp/peppy/apptainer");
        let guest_bin = guest_dir.join("bin/apptainer");

        let version = option_env!("APPTAINER_VERSION").unwrap_or("unknown");
        let marker_name = format!(".peppy-sync-{version}");

        // Fast path: check if the version marker exists (sub-second limactl call).
        let marker_exists = Command::new("limactl")
            .args(["shell", "default", "--", "test", "-f"])
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
        let _ = Command::new("limactl")
            .args(["shell", "default", "--", "rm", "-rf"])
            .arg(&guest_dir)
            .status();

        // Create the target directory in the guest.
        let mkdir = Command::new("limactl")
            .args(["shell", "default", "--", "mkdir", "-p"])
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
        let tar_pipe = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "tar -cf - -C {} . | limactl shell default -- tar -xf - -C {}",
                shell_escape(host_dir),
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
        let _ = Command::new("limactl")
            .args(["shell", "default", "--", "touch"])
            .arg(guest_dir.join(&marker_name))
            .status();

        Ok(guest_bin)
    }

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
}
