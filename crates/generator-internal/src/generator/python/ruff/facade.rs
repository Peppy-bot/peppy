use std::path::Path;
use std::process::Command;

/// Path to the ruff binary, injected at compile time by `build.rs`.
const RUFF_BINARY_PATH: Option<&str> = option_env!("RUFF_BINARY_PATH");

/// Facade for the `ruff` Python linter/formatter binary.
///
/// Uses the ruff binary built from source by `build.rs`. No runtime filesystem
/// discovery is performed — the binary path is baked in at compile time, making
/// the result fully portable.
#[derive(Debug)]
pub struct RuffFacade {
    ruff_path: &'static str,
}

impl RuffFacade {
    /// Creates a new `RuffFacade`, returning an error if the ruff binary was
    /// not embedded at compile time by `build.rs`.
    pub fn new() -> std::io::Result<Self> {
        let ruff_path = RUFF_BINARY_PATH.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "ruff binary not found. The build script (build.rs) should have built it from source.",
            )
        })?;
        Ok(Self { ruff_path })
    }

    /// Formats Python files at the given path using `ruff format`.
    pub fn format(&self, path: &Path) -> std::io::Result<()> {
        let output = Command::new(self.ruff_path)
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
        let output = Command::new(self.ruff_path)
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
