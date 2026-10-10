//! Replacing a file whole, so that a process reading it at the same time gets
//! either the old contents or the new ones, never a partial file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Replaces `path` with `contents`, which end with `mode` exactly whatever
/// the umask.
///
/// The contents are staged in a sibling file through
/// [`publish_atomic`](daemon_config::atomic_write::publish_atomic), which
/// creates the sibling readable by its owner alone, syncs it to disk after it
/// gets `mode`, and renames it over `path`. A reader therefore gets the old
/// file or the new one, and a power loss right after the rename never leaves
/// a file whose data did not reach the disk. Each call stages in a sibling of
/// its own, so writers of the same path in several threads never share one;
/// the last rename wins. When a step fails, the sibling is removed.
pub(crate) fn replace_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    daemon_config::atomic_write::publish_atomic(path, |staged| {
        fs::write(staged, contents)?;
        fs::set_permissions(staged, fs::Permissions::from_mode(mode))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptainer::scratch_dir;

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
        let scratch = scratch_dir();
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
        let scratch = scratch_dir();
        for (name, mode) in [("private.json", 0o600), ("list.conf", 0o644)] {
            let path = scratch.path().join(name);

            replace_file(&path, b"{}", mode).unwrap();

            let got = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "{name}");
        }
    }

    #[test]
    fn a_failed_rename_leaves_the_target_and_no_sibling() {
        let scratch = scratch_dir();
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
