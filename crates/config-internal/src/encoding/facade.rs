use crate::error::{Error, Result};
use capnpc::CompilerCommand;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Facade around the Cap'n Proto CLI binary.
/// Provides helpers to locate the binary bundled with the crate or built at compile time
/// and exposes higher-level operations such as compiling schemas.
#[derive(Debug, Clone)]
pub struct CapnpFacade {
    capnp_path: PathBuf,
}

impl CapnpFacade {
    /// Locate the Cap'n Proto binary either via runtime environment, compile-time build script,
    /// or the pre-bundled tools shipped with the crate.
    pub fn new() -> Result<Self> {
        let capnp_path = Self::resolve_capnp_binary()?;
        Ok(Self { capnp_path })
    }

    /// Creates a new facade using an explicit path, validating it exists.
    pub fn with_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = Self::validate_path(path.into())?;
        Ok(Self { capnp_path: path })
    }

    /// Returns the path to the Cap'n Proto binary managed by this facade.
    pub fn binary_path(&self) -> &Path {
        &self.capnp_path
    }

    /// Spawns a [`Command`] pre-configured with the Cap'n Proto binary.
    pub fn command(&self) -> Command {
        Command::new(&self.capnp_path)
    }

    /// Executes the Cap'n Proto binary with the provided arguments and returns the captured output.
    /// The call fails when the process cannot be spawned or exits with a non-zero status.
    pub fn run<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command();
        command.args(args);
        let output = command.output().map_err(Error::from)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(Error::Encoding(format!(
                "capnp exited with status {}",
                output.status
            )))
        }
    }

    /// Queries the Cap'n Proto version by calling `capnp --version`.
    pub fn version(&self) -> Result<String> {
        let output = self.run(["--version"])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim().to_string())
    }

    /// Configures a [`CompilerCommand`] to use this facade's binary.
    pub fn configure_compiler_command(&self, command: &mut CompilerCommand) {
        command.capnp_executable(&self.capnp_path);
    }

    /// Compiles the provided Cap'n Proto schemas into Rust modules under the given `output_dir`.
    /// A `capnp` subdirectory is created to host the generated sources and a `capnp.rs`
    /// module file is emitted to re-export every generated module.
    pub fn compile_files<P, O>(&self, capnp_files: &[P], output_dir: O) -> Result<()>
    where
        P: AsRef<Path>,
        O: AsRef<Path>,
    {
        Self::compile_with_executable(capnp_files, output_dir.as_ref(), self.binary_path())
    }

    fn compile_with_executable<P>(
        capnp_files: &[P],
        output_dir: &Path,
        capnp_executable: &Path,
    ) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let output_dir = output_dir.to_path_buf();
        let capnp_output_dir = output_dir.join("capnp");
        std::fs::create_dir_all(&capnp_output_dir)?;

        let mut module_exports: Vec<String> = Vec::with_capacity(capnp_files.len());
        for capnp_file in capnp_files {
            let capnp_path = capnp_file.as_ref();

            let mut command = CompilerCommand::new();
            command.capnp_executable(capnp_executable);
            command.output_path(&capnp_output_dir);
            command.default_parent_module(vec!["capnp".to_string()]);

            if let Some(parent) = capnp_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                command.src_prefix(parent);
            }

            command.file(capnp_path);

            command
                .run()
                .map_err(|err| Error::Encoding(format!("failed to run capnp compiler: {err}")))?;

            if let Some(module_name) = capnp_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(|name| format!("pub mod {}_capnp;", name))
            {
                module_exports.push(module_name);
            }
        }

        module_exports.sort();
        module_exports.dedup();

        let capnp_rs_path = output_dir.join("capnp.rs");
        let capnp_rs_content = module_exports.join("\n") + "\n";
        std::fs::write(&capnp_rs_path, capnp_rs_content)?;
        Ok(())
    }

    fn resolve_capnp_binary() -> Result<PathBuf> {
        // 1. Runtime env var takes precedence (hard error if explicitly set but missing).
        if let Some(path) = env::var_os("CAPNP_BINARY_PATH") {
            return Self::validate_path(PathBuf::from(path));
        }

        // 2. Compile-time path: only use if it still exists on disk.
        //    When the binary was built on a different machine, this path is stale.
        if let Some(path) = option_env!("CAPNP_BINARY_PATH") {
            let p = PathBuf::from(path);
            if p.is_file() {
                return Self::validate_path(p);
            }
            // Stale compile-time path — fall through to bundled binary.
        }

        // 3. Development/Test environment: check relative to manifest directory if available.
        if let Ok(manifest_dir) = env::var("CARGO_MANIFEST_DIR") {
            let tools_dir = PathBuf::from(manifest_dir).join("tools");
            let binary_name = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
                "capnp_linux_x86_64"
            } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
                "capnp_linux_aarch64"
            } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                "capnp_macos_aarch64"
            } else {
                ""
            };

            if !binary_name.is_empty() {
                let path = tools_dir.join(binary_name);
                if path.exists() {
                    return Self::validate_path(path);
                }
            }
        }

        // 4. Embedded/bundled binary (the production path for distributed builds).
        Self::validate_path(Self::bundled_capnp_binary()?)
    }

    fn validate_path(path: PathBuf) -> Result<PathBuf> {
        if path.exists() {
            Self::ensure_executable(&path)?;
            Ok(path)
        } else {
            Err(Error::Encoding(format!(
                "capnp binary not found at {}",
                path.display()
            )))
        }
    }

    fn ensure_executable(path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let metadata = fs::metadata(path).map_err(|err| {
                Error::Encoding(format!(
                    "failed to inspect capnp binary at {}: {err}",
                    path.display()
                ))
            })?;

            if metadata.is_file() {
                let mut permissions = metadata.permissions();
                let current_mode = permissions.mode();
                if current_mode & 0o111 == 0 {
                    permissions.set_mode(current_mode | 0o755);
                    fs::set_permissions(path, permissions).map_err(|err| {
                        Error::Encoding(format!(
                            "failed to mark capnp binary at {} executable: {err}",
                            path.display()
                        ))
                    })?;
                }
            }
        }

        #[cfg(not(unix))]
        let _ = path;

        Ok(())
    }

    fn bundled_capnp_binary() -> Result<PathBuf> {
        mod embedded {
            include!(concat!(env!("OUT_DIR"), "/embedded_capnp.rs"));
        }

        let binary_bytes = embedded::CAPNP_BINARY.ok_or_else(|| {
            Error::Encoding(format!(
                "no bundled capnp binary available for {}/{}",
                env::consts::OS,
                env::consts::ARCH
            ))
        })?;

        let temp_dir = std::env::temp_dir();
        let binary_path = temp_dir.join("peppy_capnp_binary");

        if !binary_path.exists() {
            std::fs::write(&binary_path, binary_bytes).map_err(|err| {
                Error::Encoding(format!("failed to write embedded capnp binary: {err}"))
            })?;
        }

        Ok(binary_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_capnp_exists() {
        let path = CapnpFacade::bundled_capnp_binary().expect("bundled capnp binary should exist");
        assert!(
            path.exists(),
            "expected bundled capnp binary at {}",
            path.display()
        );
    }
}
