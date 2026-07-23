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
    // Callers populate the temporary file through its path, so close the
    // descriptor opened by NamedTempFile before handing that path to them.
    // In particular, this ensures an executable is never published while the
    // staging descriptor is still open for writing, which Linux rejects with
    // ETXTBSY when the executable is launched immediately after publication.
    let tmp_path = tmp.into_temp_path();
    write(tmp_path.as_ref())?;
    tmp_path.persist(final_path).map_err(|e| e.error)?;
    Ok(final_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

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

    #[cfg(target_os = "linux")]
    #[test]
    fn closes_the_staging_descriptor_before_the_write_callback() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("executable");

        publish_atomic(&target, |tmp| {
            std::fs::write(tmp, b"#!/bin/sh\nexit 0\n")?;
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))?;

            let status = std::process::Command::new(tmp).status()?;
            assert!(status.success(), "staged executable should run");
            Ok(())
        })
        .expect("publish executable");
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
