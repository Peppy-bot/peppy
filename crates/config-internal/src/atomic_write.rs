//! Atomic file publication helper used across the workspace.
//!
//! All four current call sites (`repo` cache writes, build-artifact
//! publication, embedded-binary extraction) follow the same pattern:
//! create the parent dir, stage to a unique sibling tmp file, then
//! rename. Centralizing avoids drift across hand-rolled copies.

use std::path::{Path, PathBuf};

/// Stage `final_path`'s contents through a unique sibling tmp file and
/// atomically rename into place. The `write` closure receives the tmp
/// path and is responsible for creating and populating the file (and,
/// if needed, setting permissions).
///
/// Concurrent readers never observe a partial file, and concurrent
/// writers don't race over a shared staging path. Staging in the same
/// directory keeps the rename on the same filesystem (cross-fs
/// `rename(2)` returns `EXDEV`). On any error — closure failure or
/// rename failure — the tmp file is removed before returning.
pub fn publish_atomic<F>(final_path: &Path, write: F) -> std::io::Result<PathBuf>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let parent = final_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("final path has no parent: {}", final_path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    // `NamedTempFile::new_in` produces a unique sibling and deletes it
    // on drop, so a panic or early return between stage and rename
    // doesn't leave a stray.
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    let tmp_path = tmp.path().to_path_buf();
    write(&tmp_path)?;
    tmp.persist(final_path).map_err(|e| e.error)?;
    Ok(final_path.to_path_buf())
}
