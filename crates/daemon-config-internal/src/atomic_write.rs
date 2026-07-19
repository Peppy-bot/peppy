//! Atomic file publication helpers used across the workspace.
//!
//! Callers (`repo` cache writes, build-artifact publication, embedded-binary
//! extraction, credential and federation secret files) all follow the same
//! pattern: create the parent dir, stage to a unique sibling tmp file, then
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
/// `rename(2)` returns `EXDEV`). On any error (closure failure or
/// rename failure), the tmp file is removed before returning.
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

/// Atomically publishes secret bytes with owner-only permissions: the parent
/// directory is restricted to `0700` and the file is staged, chmodded to
/// `0600`, then renamed into place, so no reader ever observes the secret
/// with looser permissions.
pub fn publish_atomic_private(final_path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
        restrict_dir(parent)?;
    }
    publish_atomic(final_path, |temporary| {
        std::fs::write(temporary, content)?;
        restrict_file(temporary)
    })?;
    Ok(())
}

/// Restricts a file to owner-only read/write (`0600`). No-op off Unix.
#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Restricts a directory to owner-only access (`0700`). No-op off Unix.
#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[cfg(unix)]
    #[test]
    fn publish_atomic_private_restricts_the_file_and_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("secrets").join("key.pem");

        publish_atomic_private(&target, b"secret").expect("publish");

        assert_eq!(std::fs::read(&target).expect("read back"), b"secret");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&target), 0o600);
        assert_eq!(mode(target.parent().unwrap()), 0o700);
    }

    #[test]
    fn writes_contents_and_returns_final_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("out.txt");

        let returned =
            publish_atomic(&target, |tmp| std::fs::write(tmp, b"hello")).expect("publish");

        assert_eq!(returned, target);
        assert_eq!(std::fs::read(&target).expect("read back"), b"hello");
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Two levels that do not exist yet; publish_atomic must create them.
        let target = dir.path().join("nested").join("deeper").join("out.txt");

        publish_atomic(&target, |tmp| std::fs::write(tmp, b"x")).expect("publish");

        assert!(target.exists(), "expected {} to exist", target.display());
    }

    #[test]
    fn leaves_no_file_when_the_write_closure_fails() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("out.txt");

        let err = publish_atomic(&target, |_tmp| Err(std::io::Error::other("boom")))
            .expect_err("closure failure should propagate");

        assert_eq!(err.kind(), ErrorKind::Other);
        // The staging tmp file is cleaned up on drop and nothing was renamed
        // into place, so the destination must not exist.
        assert!(!target.exists(), "partial file must not be published");
        // The staging tmp file must not linger in the parent directory either.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected no staging leftovers, found {leftovers:?}"
        );
    }

    #[test]
    fn errors_when_final_path_has_no_parent() {
        // The filesystem root has no parent, so staging a sibling is impossible.
        let err = publish_atomic(Path::new("/"), |tmp| std::fs::write(tmp, b"x"))
            .expect_err("root path has no parent");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
