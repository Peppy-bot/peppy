use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
        let output = self.run(&["format", "--quiet"], path)?;

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
        let output = self.run(&["check", "--fix", "--quiet"], path)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(std::io::Error::other(format!(
                "ruff check --fix failed:\nstderr: {}\nstdout: {}",
                stderr.trim(),
                stdout.trim()
            )));
        }

        Ok(())
    }

    /// Runs the bundled `ruff` with `args` against `path`, retrying transient
    /// cross-process exec failures.
    ///
    /// The bundled binary is extracted to one machine-global temp path
    /// (`$TMPDIR/peppy_ruff_binary_<version>`) shared by every peppy process. When
    /// several processes — e.g. the test binaries `cargo test` runs in parallel —
    /// extract it on a cold cache, one can `execve` the file while another extractor
    /// still holds it open for writing, and the kernel returns `ETXTBSY` ("Text file
    /// busy"). That handle closes within milliseconds, so a bounded backoff-retry
    /// clears it deterministically. The `OnceLock` in `bundled_ruff_binary` only
    /// dedups extraction *within* a process; it cannot serialize separate processes,
    /// which is why the guard for that race lives here at the exec.
    fn run(&self, args: &[&str], path: &Path) -> std::io::Result<std::process::Output> {
        // ~50 attempts with a 10ms→100ms capped backoff (worst case a few seconds,
        // never reached in practice — the race clears in one or two retries).
        const MAX_RETRIES: u32 = 50;

        let mut attempt = 0u32;
        loop {
            match Command::new(&self.ruff_path).args(args).arg(path).output() {
                Ok(output) => return Ok(output),
                Err(e) if is_transient_exec_error(&e) && attempt < MAX_RETRIES => {
                    attempt += 1;
                    let backoff = Duration::from_millis((10 * attempt as u64).min(100));
                    std::thread::sleep(backoff);
                }
                Err(e) => return Err(e),
            }
        }
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
        use std::sync::OnceLock;

        // Extract the embedded binary to disk exactly once per process. Multiple
        // test threads share a PID, so without this guard they'd race on the shared
        // temp path. This only dedups *within* a process, though — separate peppy
        // processes (e.g. parallel `cargo test` binaries) still extract the same
        // machine-global path concurrently, so the exec is retried on the resulting
        // transient ENOENT / ETXTBSY in `RuffFacade::run`.
        static EXTRACTED: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();

        let result = EXTRACTED
            .get_or_init(|| Self::extract_bundled_ruff_binary().map_err(|e| e.to_string()));

        match result {
            Ok(path) => Ok(path.clone()),
            Err(msg) => Err(std::io::Error::other(msg.clone())),
        }
    }

    fn extract_bundled_ruff_binary() -> std::io::Result<PathBuf> {
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
        let binary_path = temp_dir.join(format!("peppy_ruff_binary_{}", env!("RUFF_VERSION")));

        if !binary_path.exists() {
            let result = daemon_config::atomic_write::publish_atomic(&binary_path, |tmp_path| {
                std::fs::write(tmp_path, binary_bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(tmp_path)?.permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(tmp_path, perms)?;
                }
                Ok(())
            });
            // Tolerate a lost rename race against another process — if the
            // file is now in place, that's the outcome we wanted.
            if let Err(e) = result
                && !binary_path.exists()
            {
                return Err(e);
            }
        }

        Ok(binary_path)
    }
}

/// Whether a failed `execve` of the bundled binary is a transient cross-process
/// race worth retrying. `ETXTBSY` (errno 26 on Linux/macOS/BSD) means another
/// extractor still holds the file open for writing; `NotFound` is a momentary view
/// of the published path mid-replace. Both clear within milliseconds. Anything else
/// (a real `EACCES`, a corrupt binary) is returned to the caller unchanged.
fn is_transient_exec_error(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    // No stable `ErrorKind` maps to ETXTBSY, so match the raw errno (26 across
    // Linux/macOS/BSD). Guarded to unix so a same-numbered errno elsewhere can't
    // masquerade as it.
    cfg!(unix) && e.raw_os_error() == Some(26)
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
    #[cfg(unix)]
    fn classifies_etxtbsy_and_enoent_as_transient() {
        // ETXTBSY (26): a concurrent extractor holds the binary open for writing.
        assert!(is_transient_exec_error(&std::io::Error::from_raw_os_error(
            26
        )));
        // ENOENT (2): the published path observed mid-replace.
        assert!(is_transient_exec_error(&std::io::Error::from_raw_os_error(
            2
        )));
        // EACCES (13) is a genuine failure — never retried.
        assert!(!is_transient_exec_error(
            &std::io::Error::from_raw_os_error(13)
        ));
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
