use crate::{Error, Result};
use capnpc::CompilerCommand;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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

    /// Returns the path to the Cap'n Proto binary managed by this facade.
    pub fn binary_path(&self) -> &Path {
        &self.capnp_path
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
        // An explicit override wins and must fail loudly: it is an opt-in escape
        // hatch for development and testing, not a silent fallback to whatever
        // capnp happens to be installed on the system.
        if let Some(raw) = env::var_os("CAPNP_BINARY_PATH") {
            let path = PathBuf::from(raw);
            if !path.exists() {
                return Err(Error::Encoding(format!(
                    "CAPNP_BINARY_PATH is set to {}, which does not exist",
                    path.display()
                )));
            }
            Self::ensure_executable(&path)?;
            Self::verify_runs(&path)?;
            return Ok(path);
        }

        // Otherwise use the capnp binary peppy ships and extracts itself. peppy
        // never consults a system capnp on PATH.
        Self::bundled_capnp_binary()
    }

    /// Runs `<path> --version` to prove the binary is actually executable on
    /// this host before it is handed to the capnp compiler. Transient `ETXTBSY`
    /// errors are retried because another thread can fork while the freshly
    /// extracted executable is briefly open for writing. A wrong-architecture
    /// binary is rejected by the kernel with `ENOEXEC` here, which lets us report
    /// the real path and errno instead of the capnp compiler's later, generic
    /// "install capnp" message.
    fn verify_runs(path: &Path) -> Result<()> {
        const MAX_TRANSIENT_RETRIES: u32 = 50;

        let mut attempt = 0;
        let output = loop {
            match Command::new(path).arg("--version").output() {
                Ok(output) => break output,
                Err(err)
                    if Self::is_transient_exec_error(&err) && attempt < MAX_TRANSIENT_RETRIES =>
                {
                    attempt += 1;
                    let backoff = Duration::from_millis((10 * attempt as u64).min(100));
                    std::thread::sleep(backoff);
                }
                Err(err) => {
                    return Err(Error::Encoding(format!(
                        "capnp binary at {} failed to execute on {}/{}: {err}",
                        path.display(),
                        env::consts::OS,
                        env::consts::ARCH
                    )));
                }
            }
        };

        if !output.status.success() {
            return Err(Error::Encoding(format!(
                "capnp binary at {} exited with {} when run with `--version`",
                path.display(),
                output.status
            )));
        }

        Ok(())
    }

    /// `ETXTBSY` has no stable [`std::io::ErrorKind`] variant. Its errno is 26
    /// on the Unix targets peppy supports.
    fn is_transient_exec_error(error: &std::io::Error) -> bool {
        cfg!(unix) && error.raw_os_error() == Some(26)
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
        use std::sync::OnceLock;

        // Extract-and-verify runs exactly once per process. Multiple test
        // threads share the same PID, so without this guard they race on the
        // same file and hit ENOENT / ETXTBSY.
        static RESOLVED: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();

        let result =
            RESOLVED.get_or_init(|| Self::ensure_bundled_binary().map_err(|e| e.to_string()));

        match result {
            Ok(path) => Ok(path.clone()),
            Err(msg) => Err(Error::Encoding(msg.clone())),
        }
    }

    /// Returns whether the file at `path` is byte-for-byte the `expected`
    /// embedded binary. A cheap size check runs first so a mismatched file
    /// (typically a different capnp version from an earlier install) is rejected
    /// without reading its contents; only a same-size file is fully compared.
    fn matches_embedded(path: &Path, expected: &[u8]) -> bool {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.len() == expected.len() as u64 => std::fs::read(path)
                .map(|actual| actual.as_slice() == expected)
                .unwrap_or(false),
            _ => false,
        }
    }

    fn accept_publication_result(
        binary_path: &Path,
        binary_bytes: &[u8],
        result: std::io::Result<PathBuf>,
    ) -> Result<()> {
        if let Err(err) = result
            && !Self::matches_embedded(binary_path, binary_bytes)
        {
            return Err(Error::Encoding(format!(
                "failed to install bundled capnp binary at {}: {err}",
                binary_path.display()
            )));
        }
        Ok(())
    }

    fn install_bundled_binary(binary_path: &Path, binary_bytes: &[u8]) -> Result<PathBuf> {
        // Reuse an already-extracted binary only if it matches the bytes this
        // build embeds and still runs. A stale file left by an earlier install
        // (different capnp version, or a broken/wrong-arch binary) fails the
        // content comparison or the run probe and is re-extracted rather than
        // trusted, which replaces the old "never overwrite" guard.
        if Self::matches_embedded(binary_path, binary_bytes)
            && Self::verify_runs(binary_path).is_ok()
        {
            return Ok(binary_path.to_path_buf());
        }

        let result = daemon_config::atomic_write::publish_atomic(binary_path, |tmp_path| {
            std::fs::write(tmp_path, binary_bytes)?;
            #[cfg(unix)]
            {
                fs::set_permissions(tmp_path, fs::Permissions::from_mode(0o755))?;
            }
            Ok(())
        });
        // A concurrent process may have published the same embedded bytes
        // while this process was staging its file. Accept that one race only;
        // every other publication failure must remain visible.
        Self::accept_publication_result(binary_path, binary_bytes, result)?;

        // Re-check after publication so a competing stale writer cannot make
        // this process return a path whose contents differ from its embedded
        // artifact.
        if !Self::matches_embedded(binary_path, binary_bytes) {
            return Err(Error::Encoding(format!(
                "installed capnp binary at {} does not match the binary embedded in peppy",
                binary_path.display()
            )));
        }

        // Prove the freshly written binary runs on this host. This is where a
        // wrong-arch embedded binary is caught, with guidance pointed at the
        // real fix rather than at installing a system capnp peppy will not use.
        Self::verify_runs(binary_path).map_err(|err| {
            Error::Encoding(format!(
                "{err}. This capnp is bundled with peppy (peppy does not use a \
                 system capnp); the installed peppy build looks corrupt or was \
                 built for a different architecture. Reinstall peppy with \
                 `curl -fsSL https://peppy.bot/install.sh | bash` or report the \
                 broken release at https://github.com/Peppy-bot/peppy/issues"
            ))
        })?;

        Ok(binary_path.to_path_buf())
    }

    fn ensure_bundled_binary() -> Result<PathBuf> {
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

        let binary_path = daemon_config::consts::PeppyDirs::default()
            .bin_dir()
            .join("peppy_capnp_binary");
        Self::install_bundled_binary(&binary_path, binary_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_capnp_exists() {
        let path = CapnpFacade::bundled_capnp_binary().expect("bundled capnp binary should exist");
        let expected = daemon_config::consts::PeppyDirs::default()
            .bin_dir()
            .join("peppy_capnp_binary");
        assert_eq!(path, expected);
        assert!(
            path.exists(),
            "expected bundled capnp binary at {}",
            path.display()
        );
        CapnpFacade::verify_runs(&path).expect("installed bundled capnp binary should run");
    }

    #[test]
    fn matches_embedded_requires_identical_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let embedded = b"capnp v1 bytes";

        let missing = dir.path().join("absent");
        assert!(
            !CapnpFacade::matches_embedded(&missing, embedded),
            "a missing file cannot match"
        );

        let wrong_size = dir.path().join("wrong_size");
        std::fs::write(&wrong_size, b"capnp v2 longer bytes").expect("write");
        assert!(
            !CapnpFacade::matches_embedded(&wrong_size, embedded),
            "a different-length file (e.g. a new capnp version) must be rejected"
        );

        let same_size_diff = dir.path().join("same_size_diff");
        std::fs::write(&same_size_diff, b"capnp X bytes!").expect("write");
        assert_eq!(
            same_size_diff.metadata().unwrap().len(),
            embedded.len() as u64
        );
        assert!(
            !CapnpFacade::matches_embedded(&same_size_diff, embedded),
            "a same-length but differing file must be rejected"
        );

        let identical = dir.path().join("identical");
        std::fs::write(&identical, embedded).expect("write");
        assert!(
            CapnpFacade::matches_embedded(&identical, embedded),
            "a byte-for-byte identical file must match"
        );
    }

    #[cfg(unix)]
    fn write_executable(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        path
    }

    #[cfg(unix)]
    #[test]
    fn verify_runs_reports_the_path_for_a_non_runnable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Bytes that are neither a valid binary for this host nor a script, so
        // running them fails just like a wrong-arch capnp does. Depending on the
        // platform this surfaces as an exec error (ENOEXEC) or, via the libc
        // execvp `/bin/sh` fallback, a non-zero exit. Both must be rejected and
        // must name the path, which is what we assert.
        let path = write_executable(
            dir.path(),
            "not_a_binary",
            b"\x00\x01\x02 definitely not exec",
        );

        let err = CapnpFacade::verify_runs(&path).expect_err("non-runnable file should fail");
        let message = err.to_string();

        assert!(
            message.contains(&path.display().to_string()),
            "error should name the path it tried, got: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_runs_accepts_a_binary_that_runs_successfully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(dir.path(), "ok", b"#!/bin/sh\nexit 0\n");

        CapnpFacade::verify_runs(&path).expect("a script that exits 0 should verify");
    }

    #[cfg(unix)]
    #[test]
    fn verify_runs_rejects_a_binary_that_exits_nonzero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(dir.path(), "boom", b"#!/bin/sh\nexit 3\n");

        let err = CapnpFacade::verify_runs(&path).expect_err("a failing exit should not verify");
        assert!(
            err.to_string().contains("exited with"),
            "error should report the unsuccessful exit, got: {err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn verify_runs_retries_a_temporarily_busy_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(dir.path(), "busy", b"#!/bin/sh\nexit 0\n");
        let writable = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("hold executable open for writing");
        let verify_path = path.clone();

        let verifier = std::thread::spawn(move || CapnpFacade::verify_runs(&verify_path));
        std::thread::sleep(Duration::from_millis(100));
        drop(writable);

        verifier
            .join()
            .expect("verification thread should not panic")
            .expect("verification should retry after ETXTBSY");
    }

    #[cfg(unix)]
    #[test]
    fn install_bundled_binary_creates_the_managed_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("capnp");
        let expected = b"#!/bin/sh\nexit 0\n# bundled\n";

        let installed =
            CapnpFacade::install_bundled_binary(&path, expected).expect("install bundled binary");

        assert_eq!(installed, path);
        assert_eq!(
            std::fs::read(&path).expect("read installed binary"),
            expected
        );
        assert_ne!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o111,
            0,
            "installed binary must be executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_bundled_binary_replaces_runnable_stale_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(
            dir.path(),
            "capnp",
            b"#!/bin/sh\nexit 0\n# stale but runnable\n",
        );
        let expected = b"#!/bin/sh\nexit 0\n# current bundled binary\n";

        CapnpFacade::install_bundled_binary(&path, expected)
            .expect("replace stale runnable binary");

        assert_eq!(std::fs::read(path).expect("read replacement"), expected);
    }

    #[cfg(unix)]
    #[test]
    fn install_bundled_binary_repairs_non_executable_matching_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        let expected = b"#!/bin/sh\nexit 0\n";
        std::fs::write(&path, expected).expect("write non-executable binary");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod fixture");

        CapnpFacade::install_bundled_binary(&path, expected)
            .expect("repair executable permissions");

        assert_ne!(
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o111,
            0,
            "matching bytes with broken permissions must be republished"
        );
    }

    #[test]
    fn install_bundled_binary_reports_an_unusable_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        std::fs::create_dir(&path).expect("create conflicting destination directory");

        let error = CapnpFacade::install_bundled_binary(&path, b"not published")
            .expect_err("a destination directory cannot become the binary");

        assert!(
            error
                .to_string()
                .contains("failed to install bundled capnp binary"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn publication_error_is_accepted_only_when_a_concurrent_writer_installed_exact_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        let expected = b"expected bytes";
        std::fs::write(&path, expected).expect("write concurrent result");

        CapnpFacade::accept_publication_result(
            &path,
            expected,
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated publication race",
            )),
        )
        .expect("matching concurrent publication should be accepted");

        std::fs::write(&path, b"different bytes").expect("write mismatched result");
        let error = CapnpFacade::accept_publication_result(
            &path,
            expected,
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated publication race",
            )),
        )
        .expect_err("mismatched concurrent publication must be rejected");
        assert!(
            error.to_string().contains("simulated publication race"),
            "original publication error should be preserved: {error}"
        );
    }
}
