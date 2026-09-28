//! Replacing a file whole, so that a process reading it at the same time gets
//! either the old contents or the new ones, never a partial file.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Replaces `path` with `contents`.
///
/// The contents are written to a sibling file, which is then renamed over
/// `path`. The sibling is created with `mode`, which the process umask can
/// only narrow, and then set to `mode` exactly, so the file is never readable
/// under a wider mode and ends with `mode` whatever the umask. The sibling's
/// name is unique per call, not merely per process, so writers of the same
/// path in several threads never share one; the last rename wins. When a step
/// fails, the sibling is removed.
pub(crate) fn replace_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let mut staging_name = file_name.to_os_string();
    staging_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let staging = dir.join(staging_name);

    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&staging)
        .and_then(|mut file| {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.write_all(contents)
        })
        .and_then(|()| fs::rename(&staging, path));
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn scratch() -> TempDir {
        TempDir::new_in(config_test_support::test_tmp_root()).expect("create scratch dir")
    }

    /// The names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read scratch dir")
            .map(|entry| {
                entry
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_replaced_file_holds_the_new_contents_and_no_sibling_is_left() {
        let scratch = scratch();
        let path = scratch.path().join("list.conf");
        fs::write(&path, "old\n").unwrap();

        replace_file(&path, b"new\n", 0o640).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(names_in(scratch.path()), ["list.conf"]);
    }

    /// Each mode is wider than a strict umask (077) lets a new file have, so
    /// the test also holds when the tests run under one.
    #[test]
    fn the_file_ends_with_the_mode_asked_for() {
        let scratch = scratch();
        for (name, mode) in [("private.json", 0o600), ("list.conf", 0o644)] {
            let path = scratch.path().join(name);

            replace_file(&path, b"{}", mode).unwrap();

            let got = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "{name}");
        }
    }

    #[test]
    fn a_failed_rename_leaves_the_target_and_no_sibling() {
        let scratch = scratch();
        // A non-empty directory in the target's place: the sibling is written,
        // and the rename over the directory fails.
        let path = scratch.path().join("occupied");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), "kept").unwrap();

        replace_file(&path, b"new", 0o644).expect_err("a rename over a directory fails");

        assert_eq!(names_in(scratch.path()), ["occupied"]);
        assert_eq!(fs::read_to_string(path.join("inside")).unwrap(), "kept");
    }
}
