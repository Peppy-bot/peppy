use crate::error::{Error, Result};
use capnpc::CompilerCommand;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        Self::compile_with_executable(
            capnp_files,
            output_dir.as_ref(),
            self.binary_path(),
        )
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

        let mut command = CompilerCommand::new();
        command.capnp_executable(capnp_executable);
        command.output_path(&capnp_output_dir);
        command.default_parent_module(vec!["capnp".to_string()]);

        let common_parent = capnp_files
            .first()
            .and_then(|f| f.as_ref().parent())
            .filter(|p| !p.as_os_str().is_empty());

        if let Some(parent) = common_parent {
            command.src_prefix(parent);
        }

        for capnp_file in capnp_files {
            command.file(capnp_file.as_ref());
        }

        command.run().map_err(|err| {
            Error::Encoding(format!("failed to run capnp compiler: {err}"))
        })?;

        let module_exports: Vec<String> = capnp_files
            .iter()
            .filter_map(|file| {
                let path = file.as_ref();
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|name| format!("pub mod {}_capnp;", name))
            })
            .collect();

        let capnp_rs_path = output_dir.join("capnp.rs");
        let capnp_rs_content = module_exports.join("\n") + "\n";
        std::fs::write(&capnp_rs_path, capnp_rs_content)?;
        Ok(())
    }

    fn resolve_capnp_binary() -> Result<PathBuf> {
        if let Some(path) = env::var_os("CAPNP_BINARY_PATH") {
            return Self::validate_path(PathBuf::from(path));
        }

        if let Some(path) = option_env!("CAPNP_BINARY_PATH") {
            return Self::validate_path(PathBuf::from(path));
        }

        if let Some(path) = Self::bundled_capnp_binary() {
            return Ok(path);
        }

        Err(Error::Encoding(
            "capnp binary not found; install capnp or enable the build-capnp feature".into(),
        ))
    }

    fn validate_path(path: PathBuf) -> Result<PathBuf> {
        if path.exists() {
            Ok(path)
        } else {
            Err(Error::Encoding(format!(
                "capnp binary not found at {}",
                path.display()
            )))
        }
    }

    fn bundled_capnp_binary() -> Option<PathBuf> {
        let binary_name = match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64") => "capnp_linux_x86_64",
            ("linux", "aarch64") => "capnp_linux_aarch64",
            ("macos", "aarch64") => "capnp_macos_aarch64",
            _ => return None,
        };

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join(binary_name);

        if path.exists() {
            Some(path)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_capnp_exists_when_available() {
        if let Some(path) = CapnpFacade::bundled_capnp_binary() {
            assert!(
                path.exists(),
                "expected bundled capnp binary at {}",
                path.display()
            );
        }
    }
}
