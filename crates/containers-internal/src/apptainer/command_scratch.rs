//! The removal of the scratch directory of one apptainer command (see
//! [`ApptainerCommand::scratch_dir`](super::facade::ApptainerCommand::scratch_dir)).
//!
//! Apptainer removes the temporary files of a command when the command
//! ends. A command that is killed leaves them, and among them can be the
//! unpacked copy of an image, which `unsquashfs` writes with the mode that
//! each directory has in the image. The owner of a directory without the
//! write permission cannot remove an entry of it, so a removal that such a
//! directory stops gives the owner each permission on each directory and
//! removes the tree again.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The permissions of a directory that let its owner list it, enter it and
/// remove its entries.
const OWNER_PERMISSIONS: u32 = 0o700;

/// Removes the scratch directory `dir` of an apptainer command with all that
/// the command left in it. A directory that is not there is no error.
///
/// Most trees have no directory that its owner cannot write to, so the tree
/// is removed as it is first, in one walk. Only a removal that fails walks
/// the tree to open its directories, and then removes it again.
///
/// # Errors
///
/// When the directory is still there after the removal: it holds an entry
/// of another user, or the file system refuses the change. The error is the
/// one of the entry that stopped the removal.
pub fn remove_scratch_dir(dir: &Path) -> io::Result<()> {
    if remove_tree(dir).is_ok() {
        return Ok(());
    }
    give_the_owner_each_directory(dir);
    remove_tree(dir)
}

/// Removes the tree at `dir`. The result tells whether the tree is gone: a
/// tree that is not there is a success, and an entry that another process
/// removes during the walk is a failure only when the tree stays.
fn remove_tree(dir: &Path) -> io::Result<()> {
    let Err(error) = fs::remove_dir_all(dir) else {
        return Ok(());
    };
    let is_gone = matches!(
        fs::symlink_metadata(dir),
        Err(not_found) if not_found.kind() == io::ErrorKind::NotFound
    );
    if is_gone { Ok(()) } else { Err(error) }
}

/// Gives the owner each permission on `path` and on each directory below
/// it. A symbolic link is not followed, and a file keeps its mode: the
/// permissions of its directory decide who removes it.
///
/// Best-effort: an entry that cannot be read or changed (an entry of another
/// user, an entry that another process removes) does not stop the walk of
/// the other entries. The removal that follows reports what stays.
fn give_the_owner_each_directory(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mode = metadata.permissions().mode();
    if mode & OWNER_PERMISSIONS != OWNER_PERMISSIONS {
        // A directory that stays closed keeps its entries from the walk
        // below, which then reads nothing.
        let _ = fs::set_permissions(path, Permissions::from_mode(mode | OWNER_PERMISSIONS));
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            give_the_owner_each_directory(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptainer::scratch_dir;

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, Permissions::from_mode(mode)).expect("set the mode");
    }

    /// The unpacked copy of an image holds directories with the mode that
    /// they have in the image. A directory without the write permission
    /// keeps its entries from a plain removal.
    #[test]
    fn removes_a_directory_that_holds_directories_closed_to_their_owner() {
        let parent = scratch_dir();
        let scratch = parent.path().join("command");
        let read_only = scratch.join("rootfs/usr/lib");
        let closed = scratch.join("rootfs/root");
        fs::create_dir_all(&read_only).expect("create the read-only directory");
        fs::create_dir_all(closed.join("cache")).expect("create the closed directory");
        fs::write(read_only.join("libc.so"), b"library").expect("write a file");
        fs::write(closed.join("cache/entry"), b"entry").expect("write a file");
        set_mode(&read_only, 0o555);
        set_mode(&closed, 0o000);

        remove_scratch_dir(&scratch).expect("remove the scratch directory");

        assert!(!scratch.exists());
    }

    #[test]
    fn a_directory_that_is_not_there_is_no_error() {
        let parent = scratch_dir();

        remove_scratch_dir(&parent.path().join("never-made")).expect("nothing to remove");
    }

    /// A symbolic link of an unpacked image can name a directory of the
    /// host. The removal takes the link and leaves that directory as it is.
    #[test]
    fn leaves_the_target_of_a_symbolic_link_as_it_is() {
        let parent = scratch_dir();
        let outside = parent.path().join("outside");
        let scratch = parent.path().join("command");
        fs::create_dir_all(&outside).expect("create the directory outside");
        fs::create_dir_all(&scratch).expect("create the scratch directory");
        fs::write(outside.join("kept"), b"kept").expect("write a file");
        set_mode(&outside, 0o555);
        std::os::unix::fs::symlink(&outside, scratch.join("link")).expect("create the link");

        remove_scratch_dir(&scratch).expect("remove the scratch directory");

        assert!(!scratch.exists());
        assert!(outside.join("kept").exists());
        let mode = fs::metadata(&outside)
            .expect("read the mode")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
        // The test directory removes its entries when it is dropped.
        set_mode(&outside, 0o755);
    }

    /// A tree that the first removal takes whole is not walked for its
    /// modes: a file that is not a directory is removed as a tree is.
    #[test]
    fn removes_a_tree_whose_directories_are_open_to_their_owner() {
        let parent = scratch_dir();
        let scratch = parent.path().join("command");
        fs::create_dir_all(scratch.join("rootfs/usr/lib")).expect("create the tree");
        fs::write(scratch.join("rootfs/usr/lib/libc.so"), b"library").expect("write a file");

        remove_scratch_dir(&scratch).expect("remove the scratch directory");

        assert!(!scratch.exists());
    }

    /// The error of a removal is the one of the entry that stays. Root
    /// removes an entry of each directory, so the test needs a user that is
    /// not root, and it skips itself elsewhere.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_tree_that_stays_is_the_error_of_the_removal() {
        if nix::unistd::geteuid().is_root() {
            eprintln!("SKIPPING: root removes an entry of a directory that it cannot write to");
            return;
        }
        let parent = scratch_dir();
        let scratch = parent.path().join("command");
        fs::create_dir_all(&scratch).expect("create the scratch directory");
        fs::write(scratch.join("file"), b"content").expect("write a file");
        // The parent of the scratch directory is not a part of the tree, so
        // the removal does not open it: the scratch directory stays in it.
        set_mode(parent.path(), 0o555);

        let error = remove_scratch_dir(&scratch).expect_err("the scratch directory stays");

        set_mode(parent.path(), 0o755);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(scratch.exists());
    }
}
