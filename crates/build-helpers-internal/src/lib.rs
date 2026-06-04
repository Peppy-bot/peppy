use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Returns a cache directory under `~/.peppy/tmp/{suffix}`, creating it if needed.
pub fn cache_dir(suffix: &str) -> PathBuf {
    let user_home = std::env::var("HOME").expect("HOME environment variable not set");
    let cache_dir = PathBuf::from(user_home).join(".peppy/tmp").join(suffix);
    std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    cache_dir
}

/// Returns the Rust target triple for the current build from cargo env vars.
pub fn build_target_triple() -> String {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let env_abi = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (os.as_str(), arch.as_str(), env_abi.as_str()) {
        ("macos", "aarch64", _) => "aarch64-apple-darwin".to_string(),
        ("linux", "x86_64", "gnu") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64", "gnu") => "aarch64-unknown-linux-gnu".to_string(),
        _ => format!("{arch}-unknown-{os}-{env_abi}"),
    }
}

/// Runs a command and prints a cargo warning on failure. Returns `true` on success.
pub fn run_command(command: &mut Command, description: &str) -> bool {
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("cargo:warning=Failed to {description} (exit status: {status})");
            false
        }
        Err(err) => {
            println!("cargo:warning=Failed to {description}: {err}");
            false
        }
    }
}

/// Output from a streamed command execution.
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a command, streaming its stdout and stderr as `cargo:warning=` lines.
///
/// Each output line is forwarded as `cargo:warning=[{label}] {line}` so the
/// user sees real-time progress during long-running build script operations.
/// The full captured stdout and stderr are returned for post-hoc error reporting.
pub fn run_command_streaming(command: &mut Command, label: &str) -> CommandOutput {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("cargo:warning=[{label}] Failed to spawn: {e}");
            return CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };

    // Read stdout and stderr on separate threads to avoid deadlocks.
    // If both are read sequentially, a child that fills one pipe buffer
    // while we're blocked reading the other will hang indefinitely.
    fn stream_pipe(
        pipe: impl Read + Send + 'static,
        label: String,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut captured = String::new();
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                println!("cargo:warning=[{}] {}", label, line);
                captured.push_str(&line);
                captured.push('\n');
            }
            captured
        })
    }

    let stderr_thread = stream_pipe(child.stderr.take().unwrap(), label.to_string());
    let stdout_thread = stream_pipe(child.stdout.take().unwrap(), label.to_string());

    let stdout_captured = stdout_thread.join().unwrap_or_default();
    let stderr_captured = stderr_thread.join().unwrap_or_default();
    let status = child.wait().expect("Failed to wait for child process");

    if !status.success() {
        println!("cargo:warning=[{label}] Command failed with exit status: {status}");
    }

    CommandOutput {
        success: status.success(),
        stdout: stdout_captured,
        stderr: stderr_captured,
    }
}

/// Computes the SHA-256 hash of a file using the `sha2` crate. Returns `None` on I/O error.
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            println!(
                "cargo:warning=Failed to open {:?} for SHA-256 verification: {}",
                path, e
            );
            return None;
        }
    };

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let n = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(e) => {
                println!(
                    "cargo:warning=Failed to read {:?} for SHA-256 verification: {}",
                    path, e
                );
                return None;
            }
        };
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    let mut hex = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write;
        write!(hex, "{:02x}", byte).unwrap();
    }
    Some(hex)
}

/// Verifies the SHA-256 hash of a file against an expected value.
/// Returns `true` if the hash matches.
pub fn verify_sha256(path: &Path, expected: &str, label: &str) -> bool {
    let Some(actual) = sha256_file(path) else {
        return false;
    };

    if actual.eq_ignore_ascii_case(expected) {
        true
    } else {
        println!(
            "cargo:warning={} SHA-256 mismatch for {:?}: expected {}, got {}",
            label, path, expected, actual
        );
        false
    }
}

/// Downloads `url` to `dest` using `curl`. Returns `true` on success.
///
/// On failure, removes any partially written file so a later retry starts from
/// a clean slate. Reuses [`run_command`] so failures surface as `cargo:warning`.
pub fn download_file(url: &str, dest: &Path) -> bool {
    let dest_str = dest
        .to_str()
        .expect("download destination path is not valid UTF-8");
    let ok = run_command(
        Command::new("curl").args(["-fSL", "-o", dest_str, url]),
        &format!("download {url}"),
    );
    if !ok {
        std::fs::remove_file(dest).ok();
    }
    ok
}

/// Extracts a single named `entry` from the zip archive at `zip_path` and
/// writes it to `dest`. Used to pull one binary (e.g. `zenohd`) out of a
/// release archive without unpacking the whole thing.
pub fn extract_zip_entry(zip_path: &Path, entry: &str, dest: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut zip_entry = archive
        .by_name(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
    let mut buf = Vec::with_capacity(zip_entry.size() as usize);
    zip_entry.read_to_end(&mut buf)?;
    std::fs::write(dest, &buf)
}

/// Sets the unix executable bit (0o755) on `path`; a no-op on non-unix targets.
///
/// `std::fs::write` and `std::fs::copy` do not preserve the execute bit from a
/// zip archive, so a freshly extracted binary needs this before it can be run.
pub fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap_or_else(
            |e| {
                panic!(
                    "Failed to set executable permission on {}: {}",
                    path.display(),
                    e
                )
            },
        );
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Write `contents` to `path` only if the file does not already contain identical data.
///
/// Avoids bumping the file's mtime when content is unchanged, which prevents
/// cargo from detecting a spurious change via `rerun-if-changed` or
/// `include!`/`include_bytes!` tracking.
///
/// Returns `true` if the file was actually written (content changed or file was new).
pub fn write_if_changed(path: &Path, contents: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|existing| existing == contents) {
        return false;
    }
    std::fs::write(path, contents).unwrap_or_else(|e| {
        panic!("Failed to write {}: {}", path.display(), e);
    });
    true
}

/// Returns `true` if both files exist, have the same size, and identical content.
fn files_are_identical(a: &Path, b: &Path) -> bool {
    let Ok(a_meta) = std::fs::metadata(a) else {
        return false;
    };
    let Ok(b_meta) = std::fs::metadata(b) else {
        return false;
    };
    if a_meta.len() != b_meta.len() {
        return false;
    }
    let Ok(a_bytes) = std::fs::read(a) else {
        return false;
    };
    let Ok(b_bytes) = std::fs::read(b) else {
        return false;
    };
    a_bytes == b_bytes
}

/// Copy `src` to `dst` only if `dst` does not exist or differs in size/content.
///
/// Avoids bumping the destination's mtime when the content is unchanged,
/// preventing cargo from detecting a spurious change and recompiling dependents.
///
/// Returns `true` if the copy was performed.
pub fn copy_if_changed(src: &Path, dst: &Path) -> bool {
    if files_are_identical(src, dst) {
        return false;
    }
    std::fs::copy(src, dst).unwrap_or_else(|e| {
        panic!(
            "Failed to copy {} to {}: {}",
            src.display(),
            dst.display(),
            e
        );
    });
    true
}

/// Embed the `PEPPY_GIT_TAG` environment variable into the binary at compile time.
///
/// If `PEPPY_GIT_TAG` is set and non-empty (by build_release.sh), emits a
/// `cargo:rustc-env` directive so the crate can read it via `env!()`.
/// Also registers `cargo:rerun-if-env-changed` so cargo rebuilds when the
/// variable changes.
pub fn embed_git_tag() {
    if let Ok(git_tag) = std::env::var("PEPPY_GIT_TAG")
        && !git_tag.is_empty()
    {
        println!("cargo:rustc-env=PEPPY_GIT_TAG={git_tag}");
    }
    println!("cargo:rerun-if-env-changed=PEPPY_GIT_TAG");
}

/// Find the bundled capnp binary for the current host platform in `tools_dir`.
///
/// Returns `Some(path)` if a binary matching the host OS/arch exists,
/// `None` otherwise. The `tools_dir` should point to the directory containing
/// platform-specific capnp binaries (e.g. `crates/config-internal/tools/`).
pub fn find_bundled_capnp(tools_dir: &Path) -> Option<PathBuf> {
    let binary_name = host_capnp_binary_name();
    let binary_path = tools_dir.join(binary_name);
    if binary_path.exists() {
        Some(binary_path)
    } else {
        None
    }
}

fn host_capnp_binary_name() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "capnp_linux_x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "capnp_linux_aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "capnp_macos_aarch64"
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    {
        "capnp_unsupported"
    }
}

/// Acquire an exclusive file lock for serializing concurrent build invocations.
///
/// Creates the lock directory if needed, opens the lock file, and acquires
/// an exclusive lock. Returns the `File` handle — the lock is held as long
/// as the handle is alive.
pub fn acquire_file_lock(lock_path: &Path) -> std::fs::File {
    let lock_dir = lock_path
        .parent()
        .expect("lock path should include a parent directory");
    std::fs::create_dir_all(lock_dir).expect("Failed to create lock directory");

    let lock_file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("Failed to open lock file");

    lock_file.lock().expect("Failed to acquire build lock");
    lock_file
}

/// Guard that removes a directory when dropped, ignoring errors.
struct CleanupDir(PathBuf);

impl Drop for CleanupDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Compile a Rust binary from crates.io using `cargo install` with cross-compilation support.
///
/// Returns `Some(path)` to the cached binary on success, `None` on failure.
/// Uses a separate `CARGO_TARGET_DIR` to avoid lock conflicts with the outer cargo build.
pub fn cargo_install_binary(
    name: &str,
    version: &str,
    target: &str,
    cache_dir: &Path,
) -> Option<PathBuf> {
    let cached_binary = cache_dir.join(format!("{name}-{version}-{target}"));

    if cached_binary.exists() {
        println!(
            "cargo:warning=Using cached {name} binary from {:?}",
            cached_binary
        );
        return Some(cached_binary);
    }

    println!(
        "cargo:warning=Compiling {name} {version} from source for {target} (this may take several minutes)..."
    );

    let install_root = cache_dir.join(format!("{name}-install-tmp"));
    let cargo_target_dir = cache_dir.join(format!("cargo-build-{name}"));

    // Clean up any previous partial install
    if install_root.exists() {
        std::fs::remove_dir_all(&install_root).ok();
    }
    std::fs::create_dir_all(&install_root).ok();
    std::fs::create_dir_all(&cargo_target_dir).ok();

    // Guards ensure temp directories are cleaned up on all exit paths.
    let _install_guard = CleanupDir(install_root.clone());
    let _target_guard = CleanupDir(cargo_target_dir.clone());

    let crate_spec = format!("{name}@{version}");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "install",
        &crate_spec,
        "--target",
        target,
        "--root",
        install_root.to_str().unwrap(),
    ])
    .env("CARGO_TARGET_DIR", &cargo_target_dir);

    let label = format!("cargo-install-{name}");
    let output = run_command_streaming(&mut cmd, &label);
    if !output.success {
        return None;
    }

    let built_binary = install_root.join("bin").join(name);
    if !built_binary.exists() {
        println!(
            "cargo:warning=cargo install succeeded but binary not found at {:?}",
            built_binary
        );
        return None;
    }

    if let Err(e) = std::fs::copy(&built_binary, &cached_binary) {
        println!("cargo:warning=Failed to cache compiled {name} binary: {e}");
        return None;
    }

    println!("cargo:warning=Successfully compiled and cached {name} {version} for {target}");
    Some(cached_binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_captures_stdout() {
        let output = run_command_streaming(Command::new("echo").arg("hello world"), "test-echo");
        assert!(output.success);
        assert!(output.stdout.contains("hello world"));
    }

    #[test]
    fn streaming_captures_stderr() {
        let output = run_command_streaming(
            Command::new("bash").args(["-c", "echo error-output >&2"]),
            "test-stderr",
        );
        assert!(output.success);
        assert!(output.stderr.contains("error-output"));
    }

    #[test]
    fn streaming_reports_failure() {
        let output = run_command_streaming(&mut Command::new("false"), "test-fail");
        assert!(!output.success);
    }

    #[test]
    fn streaming_handles_mixed_output() {
        let output = run_command_streaming(
            Command::new("bash").args(["-c", "echo out-line; echo err-line >&2"]),
            "test-mixed",
        );
        assert!(output.success);
        assert!(output.stdout.contains("out-line"));
        assert!(output.stderr.contains("err-line"));
    }

    #[test]
    fn extract_zip_entry_roundtrips_named_entry() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        let contents = b"#!/bin/sh\necho zenohd\n";

        // Build a small archive with two entries; we only want the named one.
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).expect("create zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("readme.txt", options).expect("entry");
        writer.write_all(b"ignore me").expect("write");
        writer.start_file("zenohd", options).expect("entry");
        writer.write_all(contents).expect("write");
        writer.finish().expect("finish zip");

        let dest = dir.path().join("zenohd");
        extract_zip_entry(&zip_path, "zenohd", &dest).expect("extract should succeed");
        assert_eq!(std::fs::read(&dest).expect("read extracted"), contents);
    }

    #[test]
    fn extract_zip_entry_missing_entry_errors() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("temp dir");
        let zip_path = dir.path().join("archive.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).expect("create zip"));
        writer
            .start_file(
                "other",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .expect("entry");
        writer.write_all(b"data").expect("write");
        writer.finish().expect("finish zip");

        let dest = dir.path().join("zenohd");
        assert!(extract_zip_entry(&zip_path, "zenohd", &dest).is_err());
    }
}
