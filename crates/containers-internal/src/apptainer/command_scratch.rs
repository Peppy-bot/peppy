//! The removal of the scratch directory of one apptainer command (see
//! [`ApptainerCommand::scratch_dir`](super::facade::ApptainerCommand::scratch_dir)).
//!
//! Apptainer removes the temporary files of a command when the command
//! ends. A command that is killed leaves them, and among them can be the
//! unpacked copy of an image, which `unsquashfs` writes with the mode that
//! each directory has in the image. The owner of a directory without the
//! write permission cannot remove an entry of it, so the removal first gives
//! the owner each permission on each directory.

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
/// # Errors
///
/// When an entry of the directory cannot be removed: an entry of another
/// user, or a file system that refuses the change.
pub fn remove_scratch_dir(dir: &Path) -> io::Result<()> {
    let removed = give_the_owner_each_directory(dir).and_then(|()| fs::remove_dir_all(dir));
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Gives the owner each permission on `path` and on each directory below
/// it. A symbolic link is not followed, and a file keeps its mode: the
/// permissions of its directory decide who removes it.
fn give_the_owner_each_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    if mode & OWNER_PERMISSIONS != OWNER_PERMISSIONS {
        fs::set_permissions(path, Permissions::from_mode(mode | OWNER_PERMISSIONS))?;
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            give_the_owner_each_directory(&entry.path())?;
        }
    }
    Ok(())
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
}
