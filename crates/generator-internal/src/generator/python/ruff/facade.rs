use std::path::{Path, PathBuf};
use std::process::Command;

/// Facade for the `ruff` Python linter/formatter binary.
///
/// The ruff binary is embedded into the peppy binary at compile time via `build.rs`
/// and extracted to a temp file at runtime, making the result fully portable across
/// machines.
#[derive(Debug)]
pub struct RuffFacade {
    ruff_path: PathBuf,
}

impl RuffFacade {
    /// Creates a new `RuffFacade`, extracting the embedded ruff binary if needed.
    pub fn new() -> std::io::Result<Self> {
        let ruff_path = Self::resolve_ruff_binary()?;
        Ok(Self { ruff_path })
    }

    /// Formats Python files at the given path using `ruff format`.
    pub fn format(&self, path: &Path) -> std::io::Result<()> {
        let output = Command::new(&self.ruff_path)
            .args(["format", "--quiet"])
            .arg(path)
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "ruff format failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(())
    }

    /// Lints and auto-fixes Python files at the given path using `ruff check --fix`.
    pub fn check_and_fix(&self, path: &Path) -> std::io::Result<()> {
        let output = Command::new(&self.ruff_path)
            .args(["check", "--fix", "--quiet"])
            .arg(path)
            .output()?;

        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "ruff check --fix failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(())
    }

    fn resolve_ruff_binary() -> std::io::Result<PathBuf> {
        // 1. Runtime env var override (for development / testing).
        if let Some(path) = std::env::var_os("RUFF_BINARY_PATH") {
            let p = PathBuf::from(path);
            if p.is_file() {
                return Ok(p);
            }
        }

        // 2. Extract from embedded bytes (production path).
        Self::bundled_ruff_binary()
    }

    fn bundled_ruff_binary() -> std::io::Result<PathBuf> {
        mod embedded {
            include!(concat!(env!("OUT_DIR"), "/embedded_ruff.rs"));
        }

        let binary_bytes = embedded::RUFF_BINARY.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "no bundled ruff binary available for {}/{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            )
        })?;

        let temp_dir = std::env::temp_dir();
        let binary_path = temp_dir.join("peppy_ruff_binary");

        if !binary_path.exists() {
            std::fs::write(&binary_path, binary_bytes)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&binary_path)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&binary_path, perms)?;
            }
        }

        Ok(binary_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_succeeds() {
        let facade = RuffFacade::new();
        assert!(facade.is_ok(), "ruff binary should be built by build.rs");
    }

    #[test]
    fn bundled_ruff_binary_exists_and_is_executable() {
        let facade = RuffFacade::new().expect("ruff binary should be available");
        assert!(
            facade.ruff_path.exists(),
            "ruff binary should exist at {}",
            facade.ruff_path.display()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&facade.ruff_path)
                .expect("should read metadata")
                .permissions();
            assert!(
                perms.mode() & 0o111 != 0,
                "ruff binary should be executable"
            );
        }
    }

    #[test]
    fn format_fixes_badly_formatted_python() {
        let facade = RuffFacade::new().expect("ruff binary should be built by build.rs");

        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let py_file = dir.path().join("bad.py");

        // Badly formatted: inconsistent spacing, missing trailing newline
        std::fs::write(&py_file, "x=1\ny  =   2\nz={'a':1,  'b':  2}\n").unwrap();

        facade.format(dir.path()).expect("ruff format failed");

        let formatted = std::fs::read_to_string(&py_file).unwrap();
        assert_eq!(formatted, "x = 1\ny = 2\nz = {\"a\": 1, \"b\": 2}\n");
    }
}
