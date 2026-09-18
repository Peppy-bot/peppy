//! Filesystem helpers: cache directories, change-aware writes/copies, and locking.

use std::path::{Path, PathBuf};

/// Returns a cache directory under `~/.peppy/tmp/{suffix}`, creating it if needed.
///
/// Reads `HOME` to locate the user home; panics if `HOME` is unset or the
/// directory cannot be created.
///
/// The returned directory is a persistent build cache shared across all
/// consumer build scripts, worktrees/checkouts, and build profiles of the
/// current user. Suffixes must therefore be unique across consumer crates and
/// version-keyed when their contents are version-specific (existing
/// convention: `ruff-{version}`, `lima-{version}-{os}-{arch}`). Nothing ever
/// cleans the cache — stale entries persist until removed manually with
/// `rm -rf ~/.peppy/tmp/<suffix>`. In production runs the peppy runtime's
/// `PeppyDirs::tmp_dir()` (peppy `daemon-config`) resolves to the same
/// `~/.peppy/tmp`, so neither side may ever bulk-clean the directory.
///
/// This is deliberately rooted at `$HOME`, not the `PEPPY_HOME` override that
/// the peppy `daemon-config` crate's `peppy_root_dir` honors: it is the persistent, version-keyed
/// build-tool cache that should survive across CI runs, distinct from the
/// per-run scratch that CI redirects via `PEPPY_HOME`. Do not "fix" it to follow
/// `PEPPY_HOME`.
pub fn cache_dir(suffix: &str) -> PathBuf {
    let user_home = std::env::var("HOME").expect("HOME environment variable not set");
    cache_dir_under(Path::new(&user_home), suffix)
}

/// Implementation of [`cache_dir`] with the home directory made explicit, so
/// tests can use a temp dir instead of the real `HOME`.
fn cache_dir_under(home: &Path, suffix: &str) -> PathBuf {
    let cache_dir = home.join(".peppy/tmp").join(suffix);
    std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    cache_dir
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
pub(crate) struct CleanupDir(pub(crate) PathBuf);

impl Drop for CleanupDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_under_creates_and_returns_peppy_tmp_subdir() {
        let home = tempfile::tempdir().expect("temp dir");
        let dir = cache_dir_under(home.path(), "my-cache");
        assert_eq!(dir, home.path().join(".peppy/tmp/my-cache"));
        assert!(dir.is_dir());
    }

    #[test]
    fn cache_dir_under_supports_nested_suffixes() {
        let home = tempfile::tempdir().expect("temp dir");
        let dir = cache_dir_under(home.path(), "a/b");
        assert_eq!(dir, home.path().join(".peppy/tmp/a/b"));
        assert!(dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn set_executable_sets_mode_755() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tool");
        std::fs::write(&path, b"#!/bin/sh\n").expect("write file");
        // Start from a known non-executable mode rather than the
        // umask-dependent creation default.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("set initial mode");

        set_executable(&path);

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "Failed to set executable permission")]
    fn set_executable_panics_on_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        set_executable(&dir.path().join("no-such-file"));
    }

    #[test]
    fn write_if_changed_writes_new_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        assert!(write_if_changed(&path, b"hello"));
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn write_if_changed_skips_identical_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"hello").expect("write");
        assert!(!write_if_changed(&path, b"hello"));
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn write_if_changed_overwrites_different_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"hello").expect("write");
        assert!(write_if_changed(&path, b"world"));
        assert_eq!(std::fs::read(&path).expect("read"), b"world");
    }

    #[test]
    fn write_if_changed_truncates_to_empty_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"nonempty").expect("write");
        assert!(write_if_changed(&path, b""));
        assert_eq!(std::fs::read(&path).expect("read"), b"");
    }

    #[test]
    fn copy_if_changed_copies_when_dst_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"payload").expect("write src");
        assert!(copy_if_changed(&src, &dst));
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"payload");
    }

    #[test]
    fn copy_if_changed_skips_identical_dst() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"payload").expect("write src");
        assert!(copy_if_changed(&src, &dst));
        assert!(!copy_if_changed(&src, &dst));
    }

    #[test]
    fn copy_if_changed_recopies_when_content_differs_at_same_length() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"payload-A").expect("write src");
        assert!(copy_if_changed(&src, &dst));
        // Same length, different bytes: exercises the content comparison
        // rather than the size precheck.
        std::fs::write(&src, b"payload-B").expect("rewrite src");
        assert!(copy_if_changed(&src, &dst));
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"payload-B");
    }

    #[test]
    fn copy_if_changed_recopies_when_length_differs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"short").expect("write src");
        assert!(copy_if_changed(&src, &dst));
        std::fs::write(&src, b"much longer payload").expect("rewrite src");
        assert!(copy_if_changed(&src, &dst));
        assert_eq!(
            std::fs::read(&dst).expect("read dst"),
            b"much longer payload"
        );
    }

    #[test]
    #[should_panic(expected = "Failed to copy")]
    fn copy_if_changed_panics_when_src_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&dst, b"existing").expect("write dst");
        copy_if_changed(&dir.path().join("no-such-src"), &dst);
    }

    #[test]
    fn acquire_file_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Nested path covers the create_dir_all branch.
        let lock_path = dir.path().join("nested/dir/build.lock");

        let first = acquire_file_lock(&lock_path);
        assert!(lock_path.exists());

        let second = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open second handle");
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        drop(first);
        // Blocking acquire rather than try_lock: a child process forked by a
        // concurrently running test can inherit the lock fd and hold the
        // flock for the instant between its fork and exec, which would make
        // an immediate try_lock flake.
        second
            .lock()
            .expect("lock should be released when the first handle is dropped");
    }
}
