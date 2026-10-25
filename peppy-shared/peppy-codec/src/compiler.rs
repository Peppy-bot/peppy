//! The Cap'n Proto compiler that the codec runs to lay out a message format.
//!
//! [`binary_path`] gives the compiler of the process:
//!
//! 1. The file that `CAPNP_BINARY_PATH` names, when the variable is set: an
//!    explicit override for development and tests. A path that does not
//!    exist or does not run is an error, never a cause to look elsewhere.
//! 2. Else the compiler that the process carries: the file that
//!    [`carry_from_file`] names, else the compiler that the crate embeds
//!    with its feature `embedded-compiler`. The codec writes it once per
//!    process into the directory that [`extract_into`] names, else into
//!    `bin` of the peppy data root of a production build (`$PEPPY_HOME`, else
//!    `~/.peppy`). The name of the file holds the digest of its bytes, so
//!    processes that carry other compilers share the directory with no
//!    conflict.
//!
//! Peppy never runs a system `capnp` from `PATH`.

use std::borrow::Cow;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// The variable that names the compiler of the process, over every other
/// source.
pub const CAPNP_BINARY_PATH_ENV: &str = "CAPNP_BINARY_PATH";

/// The process has no compiler that runs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CompilerError(String);

/// The directory that [`extract_into`] names.
static EXTRACTION_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The file that [`carry_from_file`] names.
static CARRIED_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Names the directory into which the codec writes the compiler that the
/// process carries. The first call wins and later calls change nothing, and
/// a call after the first compile of the process changes nothing either.
/// The `peppy` binary names the `bin` directory of its data root.
pub fn extract_into(dir: impl Into<PathBuf>) {
    EXTRACTION_DIR.set(dir.into()).ok();
}

/// Names the file that holds the compiler that the process carries, for a
/// process whose build embeds none: the peppylib of Python gives the file
/// that peppy deploys beside it. The codec reads the file and writes its
/// bytes into the extraction directory, as it does with an embedded
/// compiler. The first call wins, as with [`extract_into`].
pub fn carry_from_file(path: impl Into<PathBuf>) {
    CARRIED_FILE.set(path.into()).ok();
}

/// The compiler of the process (see the module documentation).
pub fn binary_path() -> Result<PathBuf, CompilerError> {
    // An explicit override wins and must fail loudly: it is an opt-in escape
    // hatch for development and testing, not a silent fallback to whatever
    // capnp happens to be installed on the system.
    if let Some(raw) = env::var_os(CAPNP_BINARY_PATH_ENV) {
        let path = PathBuf::from(raw);
        if !path.exists() {
            return Err(CompilerError(format!(
                "{CAPNP_BINARY_PATH_ENV} is set to {}, which does not exist",
                path.display()
            )));
        }
        ensure_executable(&path)?;
        verify_runs(&path)?;
        return Ok(path);
    }
    carried_binary()
}

/// The compiler that the process carries, written to its extraction
/// directory once per process. Multiple test threads share the same PID,
/// so without this guard they race on the same file and hit `ENOENT` or
/// `ETXTBSY`.
fn carried_binary() -> Result<PathBuf, CompilerError> {
    static RESOLVED: OnceLock<Result<PathBuf, CompilerError>> = OnceLock::new();
    RESOLVED.get_or_init(extract_carried_binary).clone()
}

fn extract_carried_binary() -> Result<PathBuf, CompilerError> {
    let bytes = carried_bytes()?;
    let dir = extraction_dir()?;
    install_carried_binary(&dir.join(carried_file_name(&bytes)), &bytes)
}

/// The bytes of the compiler that the process carries.
fn carried_bytes() -> Result<Cow<'static, [u8]>, CompilerError> {
    if let Some(path) = CARRIED_FILE.get() {
        return std::fs::read(path).map(Cow::Owned).map_err(|err| {
            CompilerError(format!(
                "cannot read the Cap'n Proto compiler at {}: {err}",
                path.display()
            ))
        });
    }
    embedded_bytes().map(Cow::Borrowed).ok_or_else(|| {
        CompilerError(format!(
            "this process carries no Cap'n Proto compiler: peppy gives the codec API \
             and its compiler to a node whose manifest sets `execution.codec_api: true`, \
             and `{CAPNP_BINARY_PATH_ENV}` names another compiler"
        ))
    })
}

#[cfg(feature = "embedded-compiler")]
fn embedded_bytes() -> Option<&'static [u8]> {
    mod embedded {
        include!(concat!(env!("OUT_DIR"), "/embedded_capnp.rs"));
    }
    Some(embedded::CAPNP_BINARY)
}

#[cfg(not(feature = "embedded-compiler"))]
fn embedded_bytes() -> Option<&'static [u8]> {
    None
}

/// The directory that [`extract_into`] names, else `bin` of the peppy data
/// root of a production build: `$PEPPY_HOME` when it is set and not empty,
/// else `~/.peppy`.
fn extraction_dir() -> Result<PathBuf, CompilerError> {
    if let Some(dir) = EXTRACTION_DIR.get() {
        return Ok(dir.clone());
    }
    let root = env::var_os(config::consts::PEPPY_HOME_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".peppy")))
        .ok_or_else(|| {
            CompilerError(format!(
                "no directory to write the Cap'n Proto compiler to: set `{}` or `{CAPNP_BINARY_PATH_ENV}`",
                config::consts::PEPPY_HOME_ENV
            ))
        })?;
    Ok(root.join("bin"))
}

/// The name of the extracted compiler: `peppy_capnp_` and the first 16 hex
/// digits of the SHA-256 of its bytes.
fn carried_file_name(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("peppy_capnp_{hex}")
}

/// Runs `<path> --version` to prove the binary is actually executable on
/// this host before it is handed to the capnp compiler. Transient `ETXTBSY`
/// errors are retried because another thread can fork while the freshly
/// extracted executable is briefly open for writing. A wrong-architecture
/// binary is rejected by the kernel with `ENOEXEC` here, which lets us report
/// the real path and errno instead of the capnp compiler's later, generic
/// "install capnp" message.
fn verify_runs(path: &Path) -> Result<(), CompilerError> {
    const MAX_TRANSIENT_RETRIES: u32 = 50;

    let mut attempt = 0;
    let output = loop {
        match Command::new(path).arg("--version").output() {
            Ok(output) => break output,
            Err(err) if is_transient_exec_error(&err) && attempt < MAX_TRANSIENT_RETRIES => {
                attempt += 1;
                let backoff = Duration::from_millis((10 * attempt as u64).min(100));
                std::thread::sleep(backoff);
            }
            Err(err) => {
                return Err(CompilerError(format!(
                    "capnp binary at {} failed to execute on {}/{}: {err}",
                    path.display(),
                    env::consts::OS,
                    env::consts::ARCH
                )));
            }
        }
    };

    if !output.status.success() {
        return Err(CompilerError(format!(
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

fn ensure_executable(path: &Path) -> Result<(), CompilerError> {
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path).map_err(|err| {
            CompilerError(format!(
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
                    CompilerError(format!(
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

/// Returns whether the file at `path` is byte-for-byte the `expected`
/// carried binary. A cheap size check runs first so a mismatched file is
/// rejected without reading its contents; only a same-size file is fully
/// compared.
fn matches_carried(path: &Path, expected: &[u8]) -> bool {
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
) -> Result<(), CompilerError> {
    if let Err(err) = result
        && !matches_carried(binary_path, binary_bytes)
    {
        return Err(CompilerError(format!(
            "failed to install the carried capnp binary at {}: {err}",
            binary_path.display()
        )));
    }
    Ok(())
}

fn install_carried_binary(
    binary_path: &Path,
    binary_bytes: &[u8],
) -> Result<PathBuf, CompilerError> {
    // Reuse an already-extracted binary only if it matches the carried bytes
    // and still runs. A broken or wrong-arch file fails the content
    // comparison or the run probe and is extracted again rather than
    // trusted.
    if matches_carried(binary_path, binary_bytes) && verify_runs(binary_path).is_ok() {
        return Ok(binary_path.to_path_buf());
    }

    let result = config::atomic_write::publish_atomic(binary_path, |tmp_path| {
        std::fs::write(tmp_path, binary_bytes)?;
        #[cfg(unix)]
        {
            fs::set_permissions(tmp_path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    });
    // A concurrent process may have published the same carried bytes while
    // this process was staging its file. Accept that one race only; every
    // other publication failure must remain visible.
    accept_publication_result(binary_path, binary_bytes, result)?;

    // Re-check after publication so a competing writer cannot make this
    // process return a path whose contents differ from its carried compiler.
    if !matches_carried(binary_path, binary_bytes) {
        return Err(CompilerError(format!(
            "installed capnp binary at {} does not match the binary this process carries",
            binary_path.display()
        )));
    }

    // Prove the freshly written binary runs on this host. This is where a
    // wrong-arch carried binary is caught, with guidance pointed at the real
    // fix rather than at installing a system capnp peppy will not use.
    verify_runs(binary_path).map_err(|err| {
        CompilerError(format!(
            "{err}. This capnp is the one peppy carries (peppy does not use a \
             system capnp); the build that carries it looks corrupt or was \
             built for a different architecture. Reinstall peppy with \
             `curl -fsSL https://peppy.bot/install.sh | bash` or report the \
             broken release at https://github.com/Peppy-bot/peppy/issues"
        ))
    })?;

    Ok(binary_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "embedded-compiler")]
    #[test]
    fn the_embedded_compiler_is_extracted_under_the_digest_of_its_bytes() {
        let path = carried_binary().expect("the embedded compiler is extracted");
        let bytes = embedded_bytes().expect("the crate embeds a compiler");
        assert_eq!(
            path,
            extraction_dir()
                .expect("an extraction directory")
                .join(carried_file_name(bytes))
        );
        assert!(matches_carried(&path, bytes));
        verify_runs(&path).expect("the extracted compiler runs");
    }

    #[test]
    fn the_name_of_a_carried_compiler_holds_the_digest_of_its_bytes() {
        let name = carried_file_name(b"capnp v1 bytes");
        assert_eq!(name.len(), "peppy_capnp_".len() + 16);
        assert!(name.starts_with("peppy_capnp_"));
        assert_eq!(name, carried_file_name(b"capnp v1 bytes"));
        assert_ne!(name, carried_file_name(b"capnp v2 bytes"));
    }

    #[test]
    fn matches_carried_requires_identical_contents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let embedded = b"capnp v1 bytes";

        let missing = dir.path().join("absent");
        assert!(
            !matches_carried(&missing, embedded),
            "a missing file cannot match"
        );

        let wrong_size = dir.path().join("wrong_size");
        std::fs::write(&wrong_size, b"capnp v2 longer bytes").expect("write");
        assert!(
            !matches_carried(&wrong_size, embedded),
            "a different-length file (e.g. a new capnp version) must be rejected"
        );

        let same_size_diff = dir.path().join("same_size_diff");
        std::fs::write(&same_size_diff, b"capnp X bytes!").expect("write");
        assert_eq!(
            same_size_diff.metadata().unwrap().len(),
            embedded.len() as u64
        );
        assert!(
            !matches_carried(&same_size_diff, embedded),
            "a same-length but differing file must be rejected"
        );

        let identical = dir.path().join("identical");
        std::fs::write(&identical, embedded).expect("write");
        assert!(
            matches_carried(&identical, embedded),
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

        let err = verify_runs(&path).expect_err("non-runnable file should fail");
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

        verify_runs(&path).expect("a script that exits 0 should verify");
    }

    #[cfg(unix)]
    #[test]
    fn verify_runs_rejects_a_binary_that_exits_nonzero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(dir.path(), "boom", b"#!/bin/sh\nexit 3\n");

        let err = verify_runs(&path).expect_err("a failing exit should not verify");
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

        let verifier = std::thread::spawn(move || verify_runs(&verify_path));
        std::thread::sleep(Duration::from_millis(100));
        drop(writable);

        verifier
            .join()
            .expect("verification thread should not panic")
            .expect("verification should retry after ETXTBSY");
    }

    #[cfg(unix)]
    #[test]
    fn install_carried_binary_creates_the_managed_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("capnp");
        let expected = b"#!/bin/sh\nexit 0\n# bundled\n";

        let installed = install_carried_binary(&path, expected).expect("install carried binary");

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
    fn install_carried_binary_replaces_runnable_stale_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_executable(
            dir.path(),
            "capnp",
            b"#!/bin/sh\nexit 0\n# stale but runnable\n",
        );
        let expected = b"#!/bin/sh\nexit 0\n# current bundled binary\n";

        install_carried_binary(&path, expected).expect("replace stale runnable binary");

        assert_eq!(std::fs::read(path).expect("read replacement"), expected);
    }

    #[cfg(unix)]
    #[test]
    fn install_carried_binary_repairs_non_executable_matching_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        let expected = b"#!/bin/sh\nexit 0\n";
        std::fs::write(&path, expected).expect("write non-executable binary");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod fixture");

        install_carried_binary(&path, expected).expect("repair executable permissions");

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
    fn install_carried_binary_reports_an_unusable_destination() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        std::fs::create_dir(&path).expect("create conflicting destination directory");

        let error = install_carried_binary(&path, b"not published")
            .expect_err("a destination directory cannot become the binary");

        assert!(
            error
                .to_string()
                .contains("failed to install the carried capnp binary"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn publication_error_is_accepted_only_when_a_concurrent_writer_installed_exact_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capnp");
        let expected = b"expected bytes";
        std::fs::write(&path, expected).expect("write concurrent result");

        accept_publication_result(
            &path,
            expected,
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "simulated publication race",
            )),
        )
        .expect("matching concurrent publication should be accepted");

        std::fs::write(&path, b"different bytes").expect("write mismatched result");
        let error = accept_publication_result(
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
