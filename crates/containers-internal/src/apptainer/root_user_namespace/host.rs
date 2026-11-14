//! What the choice of a user namespace reads from the host: the directories
//! on the way to a path, and the answer of the kernel on one of them.

use std::fmt;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The extended attribute that holds the access control list of a file.
const ACCESS_CONTROL_LIST_ATTRIBUTE: &str = "system.posix_acl_access";

/// The program of util-linux that runs a command in new namespaces.
const UNSHARE_PROGRAM: &str = "unshare";

/// The arguments of [`UNSHARE_PROGRAM`] that give its command a user
/// namespace which maps the user and the group of the process to root and
/// nothing else, as the user namespace of apptainer does for a root daemon.
const UNSHARE_AS_ROOT_ALONE: [&str; 3] = ["--user", "--map-root-user", "--"];

/// The shell that runs [`ENTER_DIRECTORY_SCRIPT`].
const SHELL_PROGRAM: &str = "sh";

/// A shell script that tells whether its process enters the directory of
/// its first argument. It looks for the entry `.` of the directory, which
/// the kernel finds only for a process that can enter the directory.
const ENTER_DIRECTORY_SCRIPT: &str = r#"if [ -e "$1/." ]; then echo entered; else echo closed; fi"#;

/// What [`ENTER_DIRECTORY_SCRIPT`] writes for a directory that its process
/// enters.
const SCRIPT_ANSWER_ENTERED: &str = "entered";

/// What [`ENTER_DIRECTORY_SCRIPT`] writes for a directory that its process
/// cannot enter.
const SCRIPT_ANSWER_CLOSED: &str = "closed";

/// What decides who enters a directory: its owner, its group and its
/// permission bits, and its access control list when it has one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectoryAccess {
    pub(crate) owner: u32,
    pub(crate) group: u32,
    pub(crate) mode: u32,
    /// Whether the directory has an access control list, of which an entry
    /// can admit a user where the permission bits do not. This module does
    /// not read the entries.
    pub(crate) has_access_control_list: bool,
}

/// The host as the choice of a user namespace reads it.
pub(crate) trait Host: fmt::Debug + Send + Sync {
    /// `path` with its symbolic links resolved, or `None` for a path that
    /// cannot be resolved.
    fn resolved(&self, path: &Path) -> Option<PathBuf>;

    /// What decides who enters the directory at `dir`, or `None` for a
    /// directory whose permissions cannot be read.
    fn access_of(&self, dir: &Path) -> Option<DirectoryAccess>;

    /// The answer of the kernel: whether a process that is root in a user
    /// namespace which maps user 0 and `mapped_group` only enters the
    /// directory at `dir`. `None` when the host gives no answer.
    fn root_enters_from_user_namespace(&self, dir: &Path, mapped_group: u32) -> Option<bool>;

    /// The working directory of this process, in which apptainer runs when
    /// its command names none.
    fn current_dir(&self) -> Option<PathBuf>;
}

/// The host that this process runs on.
#[derive(Debug)]
pub(crate) struct ThisHost;

impl Host for ThisHost {
    fn resolved(&self, path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok()
    }

    fn access_of(&self, dir: &Path) -> Option<DirectoryAccess> {
        let has_access_control_list = has_access_control_list(dir)?;
        let metadata = fs::metadata(dir).ok()?;
        Some(DirectoryAccess {
            owner: metadata.uid(),
            group: metadata.gid(),
            mode: metadata.mode(),
            has_access_control_list,
        })
    }

    /// Runs [`ENTER_DIRECTORY_SCRIPT`] in a user namespace of
    /// [`UNSHARE_PROGRAM`]. A host without the program, and a host that
    /// gives the process no user namespace, give no answer.
    fn root_enters_from_user_namespace(&self, dir: &Path, mapped_group: u32) -> Option<bool> {
        let output = Command::new(UNSHARE_PROGRAM)
            .args(UNSHARE_AS_ROOT_ALONE)
            .args([SHELL_PROGRAM, "-c", ENTER_DIRECTORY_SCRIPT, SHELL_PROGRAM])
            .arg(dir)
            // The program maps the group of its process, and apptainer maps
            // the real group of the daemon.
            .gid(mapped_group)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        match String::from_utf8_lossy(&output.stdout).trim() {
            SCRIPT_ANSWER_ENTERED => Some(true),
            SCRIPT_ANSWER_CLOSED => Some(false),
            _ => None,
        }
    }

    fn current_dir(&self) -> Option<PathBuf> {
        std::env::current_dir().ok()
    }
}

/// Whether the file at `path` has an access control list, or `None` when
/// the file cannot be read. A file system that has no extended attributes
/// has no list.
fn has_access_control_list(path: &Path) -> Option<bool> {
    match rustix::fs::getxattr_query(path, ACCESS_CONTROL_LIST_ATTRIBUTE) {
        Ok(_) => Some(true),
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => Some(false),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptainer::scratch_dir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_permissions_of_a_real_directory_are_read() {
        let scratch = scratch_dir();
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o750))
            .expect("set the mode of the scratch directory");

        let access = ThisHost
            .access_of(scratch.path())
            .expect("read the permissions of the scratch directory");

        assert_eq!(access.mode & 0o777, 0o750);
        assert_eq!(access.owner, rustix::process::geteuid().as_raw());
        assert!(!access.has_access_control_list);
    }

    /// A user of a host that gives it a user namespace is root in that
    /// namespace: it enters its own directory, and not the home directory of
    /// the real root. The test needs such a host and a user that is not
    /// root, and it skips itself elsewhere.
    #[test]
    fn the_kernel_tells_which_directory_root_of_a_user_namespace_enters() {
        let scratch = scratch_dir();
        let own_group = rustix::process::getgid().as_raw();
        let is_root = rustix::process::geteuid().is_root();
        let Some(enters_own_dir) = ThisHost
            .root_enters_from_user_namespace(scratch.path(), own_group)
            .filter(|_| !is_root)
        else {
            eprintln!("SKIPPING: this host gives the user of the test no user namespace");
            return;
        };

        let enters_home_of_root =
            ThisHost.root_enters_from_user_namespace(Path::new("/root"), own_group);

        assert!(enters_own_dir, "a user enters its own directory");
        assert_eq!(enters_home_of_root, Some(false));
    }

    #[test]
    fn the_permissions_of_a_directory_that_does_not_exist_are_not_read() {
        let scratch = scratch_dir();
        let missing = scratch.path().join("no-such-directory");

        assert!(ThisHost.access_of(&missing).is_none());
        assert!(ThisHost.resolved(&missing).is_none());
    }

    #[test]
    fn a_path_is_resolved_through_its_symbolic_links() {
        let scratch = scratch_dir();
        let target = scratch.path().join("target");
        let link = scratch.path().join("link");
        fs::create_dir(&target).expect("create the target directory");
        std::os::unix::fs::symlink(&target, &link).expect("create the link");

        let resolved = ThisHost.resolved(&link).expect("resolve the link");

        assert_eq!(
            resolved,
            target.canonicalize().expect("resolve the target directory")
        );
    }
}
