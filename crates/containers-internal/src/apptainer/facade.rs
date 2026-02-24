use super::super::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Facade for the Apptainer container runtime.
///
/// Apptainer is installed as a portable, relocatable directory tree (created by
/// `install-unprivileged.sh`) rather than a single binary. The facade resolves
/// the installation directory and provides command-builder methods for common
/// apptainer operations.
///
/// On macOS, commands are transparently routed through a Lima VM since apptainer
/// is Linux-only.
#[derive(Debug)]
pub struct ApptainerFacade {
    /// Root of the apptainer installation (contains `bin/`, arch dirs, etc.)
    apptainer_dir: PathBuf,
    /// Resolved path to `bin/apptainer` within the installation
    apptainer_bin: PathBuf,
    /// Whether to route commands through Lima (macOS).
    use_lima: bool,
}

impl ApptainerFacade {
    /// Creates a new `ApptainerFacade` by resolving the apptainer installation directory.
    ///
    /// Resolution order:
    /// 1. `PEPPY_APPTAINER_DIR` environment variable
    /// 2. `../apptainer/` relative to the current executable (installed layout)
    /// 3. Compile-time `APPTAINER_INSTALL_DIR` set by build.rs
    /// 4. `apptainer` found on PATH (derive parent directory)
    pub fn new() -> Result<Self> {
        let apptainer_dir = Self::resolve_apptainer_dir()?;
        let apptainer_bin = apptainer_dir.join("bin/apptainer");

        if !apptainer_bin.exists() {
            return Err(Error::ApptainerNotFound(format!(
                "bin/apptainer not found in installation directory: {}",
                apptainer_dir.display()
            )));
        }

        let use_lima = cfg!(target_os = "macos");

        Ok(Self {
            apptainer_dir,
            apptainer_bin,
            use_lima,
        })
    }

    /// Returns the root directory of the apptainer installation.
    pub fn install_dir(&self) -> &Path {
        &self.apptainer_dir
    }

    /// Returns the path to the apptainer binary.
    pub fn binary_path(&self) -> &Path {
        &self.apptainer_bin
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
    pub fn run(&self, image: &str, args: &[&str]) -> Result<Child> {
        let mut all_args = vec!["run", image];
        all_args.extend(args);
        self.spawn(&all_args)
    }

    /// Execute a command inside a container: `apptainer exec <container> <cmd...>`
    pub fn exec(&self, container: &str, cmd: &[&str]) -> Result<Child> {
        let mut all_args = vec!["exec", container];
        all_args.extend(cmd);
        self.spawn(&all_args)
    }

    /// Build a container image: `apptainer build <output> <def_file>`
    pub fn build(&self, output: &Path, def_file: &Path) -> Result<Child> {
        let output_str = output.to_string_lossy();
        let def_str = def_file.to_string_lossy();
        self.spawn(&["build", &output_str, &def_str])
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
    /// On macOS: runs `limactl shell default -- {apptainer_bin} <args...>` to
    /// execute inside the Lima VM.
    fn command(&self, args: &[&str]) -> Command {
        if self.use_lima {
            let mut cmd = Command::new("limactl");
            cmd.arg("shell").arg("default").arg("--");
            cmd.arg(&self.apptainer_bin);
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

        // 4) Search PATH for `apptainer` and derive the installation directory
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join("apptainer");
                if candidate.is_file() {
                    // The binary is at {install_dir}/bin/apptainer, so the install dir
                    // is two levels up: bin/ -> install_dir.
                    if let Some(install_dir) = dir.parent()
                        && install_dir.is_dir()
                    {
                        return Ok(install_dir.to_path_buf());
                    }
                    // If we can't derive the parent, at least return the bin dir's parent
                    return Ok(dir);
                }
            }
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
