//! Filesystem helpers: cache directories, change-aware writes/copies, and locking.

use std::path::{Path, PathBuf};

/// Returns a cache directory under `~/.peppy/tmp/{suffix}`, creating it if needed.
pub fn cache_dir(suffix: &str) -> PathBuf {
    let user_home = std::env::var("HOME").expect("HOME environment variable not set");
    let cache_dir = PathBuf::from(user_home).join(".peppy/tmp").join(suffix);
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
