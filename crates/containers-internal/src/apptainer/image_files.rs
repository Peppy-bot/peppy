//! How a container gets the files of its image: from a mount of the image,
//! or from an unpacked copy of it.
//!
//! For a user that is not root, apptainer mounts the squashfs of an image
//! with `squashfuse_ll`, a FUSE file system, in the user namespace of the
//! container. That namespace maps the user of the daemon and no other. The
//! kernel refuses a FUSE request of a user that the namespace of the mount
//! cannot map (`fuse_get_req` gives `EOVERFLOW`).
//!
//! Overlayfs does each read and each write of a file with the credentials
//! of the process that mounted it (`backing_file_write_iter` runs in
//! `scoped_with_creds`), not with the credentials of the process that reads
//! or writes. The root file system of a Docker or Podman container is on
//! overlayfs, and so is the one of a live session. Its mounter is a user
//! that the namespace of the container does not map.
//!
//! The two meet when a program of the image writes to a file on overlayfs
//! from a page of the image that is not in memory: data that is compiled
//! into the program, or a mapping of a file of the image. The kernel reads
//! the page for the write, with the credentials of the mounter. FUSE refuses
//! the read, and the write fails with `EFAULT` ("Bad address"). A read from
//! overlayfs into such a page fails the same way.
//!
//! Nothing in a container can keep its programs away from this: apptainer
//! binds the home directory, `/tmp` and `/var/tmp` of the host, the working
//! directory of its process and each bind source into the container. So a
//! daemon that is not root passes `--unsquash` for a command with one of
//! these directories on overlayfs. Apptainer then unpacks the image with
//! `unsquashfs` and runs the container from the copy, whose files are files
//! of the disk: no page of the image comes from a FUSE mount.
//!
//! A daemon that is root has no such fault. Apptainer mounts its images with
//! the kernel, or with `squashfuse_ll` in a user namespace that maps user 0
//! (see [`root_user_namespace`](super::root_user_namespace)), which is the
//! mounter of the overlayfs of its container.
//!
//! The choice reads the directories and not the flags of the command. A
//! command that keeps one of the directories out of its container
//! (`--contain`, `--no-home`, a read-only bind) gets an unpacked image that
//! it does not need, which costs time and disk space and nothing else.

use super::root_user_namespace::{ImageCommand, working_dir_of_this_process};
use super::unprivileged_user_namespace;
use nix::sys::statfs::{OVERLAYFS_SUPER_MAGIC, statfs};
use nix::unistd::{Uid, User};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// The flag of `apptainer run` and `apptainer exec` that unpacks the image
/// and runs the container from the copy.
const UNPACK_FLAG: &str = "--unsquash";

/// The directories for temporary files that apptainer binds from the host
/// into a container.
const TEMPORARY_DIRECTORIES: [&str; 2] = ["/tmp", "/var/tmp"];

/// How the containers of a daemon get the files of their images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageFiles {
    /// Apptainer mounts the image.
    Mounted,
    /// Apptainer unpacks the image and runs the container from the copy,
    /// because the container can write to these directories, which are on
    /// overlayfs.
    Unpacked {
        directories_on_overlayfs: Vec<PathBuf>,
    },
}

impl ImageFiles {
    /// How the nodes of the daemon of the user of the container builds get
    /// the files of their images: the user of this process, or the caller of
    /// `sudo` when `sudo` runs it. The answer is for the directories that
    /// each node has, of which the data root holds its working directory and
    /// its view of the data of the daemon. A node with a bind source on
    /// overlayfs gets an unpacked image where this answer is
    /// [`ImageFiles::Mounted`].
    ///
    /// The data root is the one that this process resolves for that user:
    /// `PEPPY_HOME` when this process has it, or else the default data root
    /// in the home directory of that user. `sudo` gives this process the
    /// home directory of root and no `PEPPY_HOME` of its caller, so the
    /// home directory of the caller comes from the user database.
    pub(crate) fn of_the_nodes_of_the_build_user() -> Self {
        let user = unprivileged_user_namespace::build_user_id();
        let home = home_of(user);
        let data_root = if unprivileged_user_namespace::build_user_is_the_caller_of_sudo() {
            daemon_config::consts::peppy_root_dir_of_user(home.clone())
        } else {
            daemon_config::consts::peppy_root_dir()
        };
        UnpackedImages::for_daemon(user, home, Box::new(ThisHost)).image_files(&[&data_root])
    }

    /// Why apptainer unpacks the image, as a clause that names each
    /// directory on overlayfs: "`/tmp` is on overlayfs", "`/tmp` and
    /// `/var/tmp` are on overlayfs". `None` for an image that it mounts.
    #[must_use]
    pub fn cause_of_unpacking(&self) -> Option<String> {
        let Self::Unpacked {
            directories_on_overlayfs,
        } = self
        else {
            return None;
        };
        let names: Vec<String> = directories_on_overlayfs
            .iter()
            .map(|directory| format!("`{}`", directory.display()))
            .collect();
        let (last, first) = names.split_last()?;
        if first.is_empty() {
            return Some(format!("{last} is on overlayfs"));
        }
        Some(format!("{} and {last} are on overlayfs", first.join(", ")))
    }
}

/// Which `run` and which `exec` of a daemon ask apptainer to unpack the
/// image.
#[derive(Debug)]
pub(crate) enum UnpackedImages {
    /// The daemon is root: apptainer mounts each image.
    Never,
    /// The daemon is not root: apptainer unpacks the image of a command
    /// with a directory on overlayfs.
    BesideOverlayfs(UnprivilegedDaemon),
}

impl UnpackedImages {
    /// The choice for the daemon of this process on this host. It tells the
    /// operator, once for the process, when the directories that each
    /// container has make apptainer unpack each image.
    pub(crate) fn for_this_daemon(data_root: &Path) -> Self {
        let user = nix::unistd::geteuid();
        let unpacked_images = Self::for_daemon(user, home_of(user), Box::new(ThisHost));
        static ANNOUNCED: Once = Once::new();
        ANNOUNCED.call_once(|| unpacked_images.announce(data_root));
        unpacked_images
    }

    /// [`UnpackedImages::for_this_daemon`] for a daemon and a host that the
    /// arguments describe: `home` is the home directory of `user`, or `None`
    /// for a user without one.
    fn for_daemon(user: Uid, home: Option<PathBuf>, host: Box<dyn Host>) -> Self {
        if user.is_root() {
            return Self::Never;
        }
        Self::BesideOverlayfs(UnprivilegedDaemon { home, host })
    }

    /// The flag that the `run` or the `exec` of `command` passes to
    /// apptainer, if any.
    pub(crate) fn flag_for(&self, command: &ImageCommand<'_>) -> Option<&'static str> {
        let Self::BesideOverlayfs(daemon) = self else {
            return None;
        };
        let dir_of_the_process = command.dir_of_the_process(|| daemon.host.current_dir());
        let directories: Vec<&Path> = dir_of_the_process
            .as_deref()
            .into_iter()
            .chain(command.bind_sources.iter().copied())
            .collect();
        match self.image_files(&directories) {
            ImageFiles::Mounted => None,
            ImageFiles::Unpacked { .. } => Some(UNPACK_FLAG),
        }
    }

    /// How a container gets the files of its image, when it has
    /// `directories` beside the directories that apptainer binds into each
    /// container.
    fn image_files(&self, directories: &[&Path]) -> ImageFiles {
        let Self::BesideOverlayfs(daemon) = self else {
            return ImageFiles::Mounted;
        };
        let directories_on_overlayfs = daemon.directories_on_overlayfs(directories);
        if directories_on_overlayfs.is_empty() {
            return ImageFiles::Mounted;
        }
        ImageFiles::Unpacked {
            directories_on_overlayfs,
        }
    }

    /// Tells the operator that apptainer unpacks the image of each container
    /// of this daemon, when the directories that each container has are the
    /// cause.
    fn announce(&self, data_root: &Path) {
        let Some(cause) = self.image_files(&[data_root]).cause_of_unpacking() else {
            return;
        };
        tracing::info!(
            "This daemon is not root and {cause}, so apptainer unpacks the image of each \
             container at its start ({UNPACK_FLAG}) and does not mount it: a start takes more \
             time, and the copy uses disk space until the container ends"
        );
    }
}

/// A daemon that is not root, and the host that tells the file system of a
/// directory.
#[derive(Debug)]
pub(crate) struct UnprivilegedDaemon {
    /// The home directory of the user of the daemon, which apptainer binds
    /// into each container.
    home: Option<PathBuf>,
    host: Box<dyn Host>,
}

impl UnprivilegedDaemon {
    /// The outermost directories on overlayfs among those that apptainer
    /// binds into each container (the directories for temporary files, then
    /// the home directory) and then `directories`, each one time. A
    /// directory below another of them is not in the list: a volume at the
    /// outer one holds it also.
    fn directories_on_overlayfs(&self, directories: &[&Path]) -> Vec<PathBuf> {
        let of_each_container = TEMPORARY_DIRECTORIES
            .into_iter()
            .map(Path::new)
            .chain(self.home.as_deref());
        let on_overlayfs: Vec<&Path> = of_each_container
            .chain(directories.iter().copied())
            .filter(|directory| self.host.is_on_overlayfs(directory))
            .collect();
        let mut outermost: Vec<PathBuf> = Vec::new();
        for directory in &on_overlayfs {
            let is_below_another = on_overlayfs
                .iter()
                .any(|other| other != directory && directory.starts_with(other));
            if !is_below_another && !outermost.iter().any(|listed| listed == directory) {
                outermost.push(directory.to_path_buf());
            }
        }
        outermost
    }
}

/// The home directory of `user` in the user database, which is the one that
/// apptainer binds into a container of that user.
fn home_of(user: Uid) -> Option<PathBuf> {
    User::from_uid(user).ok().flatten().map(|entry| entry.dir)
}

/// The host as the choice of the image files reads it.
pub(crate) trait Host: fmt::Debug + Send + Sync {
    /// Whether the file system that holds `path` is overlayfs. `false` for a
    /// path that the host does not have or does not let this process read:
    /// apptainer binds no such path into a container.
    fn is_on_overlayfs(&self, path: &Path) -> bool;

    /// The working directory of this process, in which apptainer runs when
    /// its command names none.
    fn current_dir(&self) -> Option<PathBuf>;
}

/// The host that this process runs on.
#[derive(Debug)]
struct ThisHost;

impl Host for ThisHost {
    fn is_on_overlayfs(&self, path: &Path) -> bool {
        statfs(path).is_ok_and(|filesystem| filesystem.filesystem_type() == OVERLAYFS_SUPER_MAGIC)
    }

    fn current_dir(&self) -> Option<PathBuf> {
        working_dir_of_this_process()
    }
}

/// What the tests of the choice and of the commands of the facade share.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// The home directory of the user of the daemon of the fixtures.
    pub(crate) const HOME: &str = "/home/operator";

    /// The working directory of the daemon of the fixtures.
    pub(crate) const DAEMON_DIR: &str = "/srv/daemon";

    /// A host with overlayfs at the listed mount points: a path is on
    /// overlayfs when the nearest mount point at or above it is one of them.
    #[derive(Debug)]
    pub(crate) struct FakeHost {
        overlayfs_mounts: Vec<&'static str>,
        other_mounts: Vec<&'static str>,
    }

    impl FakeHost {
        /// A host whose root file system is not overlayfs.
        pub(crate) fn without_overlayfs() -> Self {
            Self {
                overlayfs_mounts: Vec::new(),
                other_mounts: vec!["/"],
            }
        }

        /// A Docker container: its root file system is on overlayfs.
        pub(crate) fn docker_container() -> Self {
            Self {
                overlayfs_mounts: vec!["/"],
                other_mounts: Vec::new(),
            }
        }

        /// This host with a file system that is not overlayfs at each of
        /// `mount_points`, as a volume or a tmpfs of a container is.
        pub(crate) fn with_volumes(mut self, mount_points: &[&'static str]) -> Self {
            self.other_mounts.extend(mount_points);
            self
        }

        /// This host with overlayfs at `mount_point`.
        pub(crate) fn with_overlayfs_at(mut self, mount_point: &'static str) -> Self {
            self.overlayfs_mounts.push(mount_point);
            self
        }
    }

    impl Host for FakeHost {
        fn is_on_overlayfs(&self, path: &Path) -> bool {
            let nearest = |mounts: &[&str]| {
                mounts
                    .iter()
                    .filter(|mount| path.starts_with(mount))
                    .map(|mount| Path::new(mount).components().count())
                    .max()
            };
            nearest(&self.overlayfs_mounts) > nearest(&self.other_mounts)
        }

        fn current_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from(DAEMON_DIR))
        }
    }

    /// The choice of a daemon that is not root, with the home directory
    /// [`HOME`], on `host`.
    pub(crate) fn daemon_that_is_not_root(host: FakeHost) -> UnpackedImages {
        UnpackedImages::for_daemon(
            Uid::from_raw(1000),
            Some(PathBuf::from(HOME)),
            Box::new(host),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{DAEMON_DIR, FakeHost, HOME, daemon_that_is_not_root};
    use super::*;

    const DATA_ROOT: &str = "/home/operator/.peppy";
    const WORKING_DIR: &str = "/home/operator/.peppy/instances/camera_inst";
    const BIND_SOURCE: &str = "/data/recordings";

    fn command<'a>(working_dir: Option<&'a str>, bind_sources: &[&'a str]) -> ImageCommand<'a> {
        ImageCommand {
            image: Path::new("/images/camera.sif"),
            apptainer_dir: Path::new("/opt/apptainer"),
            working_dir: working_dir.map(Path::new),
            bind_sources: bind_sources.iter().copied().map(Path::new).collect(),
        }
    }

    fn node_command() -> ImageCommand<'static> {
        command(Some(WORKING_DIR), &[BIND_SOURCE])
    }

    fn paths(directories: &[&str]) -> Vec<PathBuf> {
        directories.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_daemon_that_is_not_root_mounts_its_images_on_a_host_without_overlayfs() {
        let unpacked_images = daemon_that_is_not_root(FakeHost::without_overlayfs());

        assert_eq!(unpacked_images.flag_for(&node_command()), None);
        assert_eq!(
            unpacked_images.image_files(&[Path::new(DATA_ROOT)]),
            ImageFiles::Mounted
        );
    }

    #[test]
    fn a_daemon_that_is_not_root_unpacks_its_images_in_a_docker_container() {
        let unpacked_images = daemon_that_is_not_root(FakeHost::docker_container());

        assert_eq!(
            unpacked_images.flag_for(&node_command()),
            Some("--unsquash")
        );
        assert_eq!(
            unpacked_images.image_files(&[Path::new(DATA_ROOT)]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/tmp", "/var/tmp", HOME]),
            }
        );
    }

    #[test]
    fn a_root_daemon_mounts_its_images_in_a_docker_container() {
        let unpacked_images = UnpackedImages::for_daemon(
            Uid::from_raw(0),
            Some(PathBuf::from("/root")),
            Box::new(FakeHost::docker_container()),
        );

        assert_eq!(unpacked_images.flag_for(&node_command()), None);
        assert_eq!(
            unpacked_images.image_files(&[Path::new("/root/.peppy")]),
            ImageFiles::Mounted
        );
    }

    /// Each directory that a container can write to makes apptainer unpack
    /// the image when overlayfs is mounted at it.
    #[test]
    fn one_directory_on_overlayfs_is_enough_to_unpack_the_image() {
        for directory in ["/tmp", "/var/tmp", HOME, WORKING_DIR, BIND_SOURCE] {
            let host = FakeHost::without_overlayfs().with_overlayfs_at(directory);

            assert_eq!(
                daemon_that_is_not_root(host).flag_for(&node_command()),
                Some("--unsquash"),
                "{directory} is on overlayfs"
            );
        }
    }

    #[test]
    fn a_container_with_a_volume_at_each_of_its_directories_mounts_its_image() {
        let host = FakeHost::docker_container().with_volumes(&["/tmp", "/var/tmp", HOME, "/data"]);
        let unpacked_images = daemon_that_is_not_root(host);

        assert_eq!(unpacked_images.flag_for(&node_command()), None);
        assert_eq!(
            unpacked_images.image_files(&[Path::new(DATA_ROOT)]),
            ImageFiles::Mounted
        );
    }

    #[test]
    fn the_status_names_only_the_directories_on_overlayfs() {
        let host = FakeHost::docker_container().with_volumes(&["/tmp", HOME]);

        assert_eq!(
            daemon_that_is_not_root(host).image_files(&[Path::new(DATA_ROOT)]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/var/tmp"]),
            }
        );
    }

    /// A command that names no working directory runs in the one of the
    /// daemon, which apptainer binds into the container.
    #[test]
    fn a_command_without_a_working_directory_has_the_one_of_the_daemon() {
        let host = FakeHost::without_overlayfs().with_overlayfs_at(DAEMON_DIR);
        let unpacked_images = daemon_that_is_not_root(host);

        assert_eq!(
            unpacked_images.flag_for(&command(None, &[])),
            Some("--unsquash")
        );
        assert_eq!(
            unpacked_images.flag_for(&command(Some("/srv/instances/camera_inst"), &[])),
            None
        );
    }

    #[test]
    fn a_user_without_a_home_directory_has_the_other_directories() {
        let host = FakeHost::without_overlayfs().with_overlayfs_at(HOME);
        let unpacked_images = UnpackedImages::for_daemon(Uid::from_raw(1000), None, Box::new(host));

        assert_eq!(
            unpacked_images.flag_for(&command(Some("/srv/instances/camera_inst"), &[])),
            None
        );
    }

    /// The data root below a home directory on overlayfs is not named: a
    /// volume at the home directory holds it also. A data root that is not
    /// below another directory of the list is named.
    #[test]
    fn a_directory_below_another_one_on_overlayfs_is_not_listed() {
        let unpacked_images = daemon_that_is_not_root(FakeHost::docker_container());

        assert_eq!(
            unpacked_images.image_files(&[Path::new(DATA_ROOT), Path::new("/tmp/builds")]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/tmp", "/var/tmp", HOME]),
            }
        );
        assert_eq!(
            unpacked_images.image_files(&[Path::new("/peppy-home")]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/tmp", "/var/tmp", HOME, "/peppy-home"]),
            }
        );
    }

    /// The order of the directories does not decide which one is named.
    #[test]
    fn the_outer_directory_is_listed_when_the_inner_one_comes_first() {
        let host = FakeHost::without_overlayfs().with_overlayfs_at("/data");
        let unpacked_images = daemon_that_is_not_root(host);

        assert_eq!(
            unpacked_images.image_files(&[Path::new("/data/recordings"), Path::new("/data")]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/data"]),
            }
        );
    }

    #[test]
    fn a_directory_that_a_command_names_twice_is_listed_one_time() {
        let unpacked_images = daemon_that_is_not_root(FakeHost::docker_container());

        assert_eq!(
            unpacked_images.image_files(&[Path::new(HOME), Path::new("/tmp")]),
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(&["/tmp", "/var/tmp", HOME]),
            }
        );
    }

    #[test]
    fn the_cause_of_unpacking_names_each_directory_on_overlayfs() {
        let cause = |directories: &[&str]| {
            ImageFiles::Unpacked {
                directories_on_overlayfs: paths(directories),
            }
            .cause_of_unpacking()
        };

        assert_eq!(cause(&["/tmp"]).as_deref(), Some("`/tmp` is on overlayfs"));
        assert_eq!(
            cause(&["/tmp", "/var/tmp"]).as_deref(),
            Some("`/tmp` and `/var/tmp` are on overlayfs")
        );
        assert_eq!(
            cause(&["/tmp", "/var/tmp", HOME]).as_deref(),
            Some("`/tmp`, `/var/tmp` and `/home/operator` are on overlayfs")
        );
        assert_eq!(ImageFiles::Mounted.cause_of_unpacking(), None);
    }

    /// The file system of `/proc` is never overlayfs, and a path that the
    /// host does not have is on no file system.
    #[test]
    fn this_host_tells_a_file_system_that_is_not_overlayfs() {
        assert!(!ThisHost.is_on_overlayfs(Path::new("/proc")));
        assert!(!ThisHost.is_on_overlayfs(Path::new("/no/such/directory")));
    }
}
