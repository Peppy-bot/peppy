//! The user namespace of the containers of a root daemon.
//!
//! Apptainer mounts the squashfs of an image in one of two ways:
//!
//! - For a user that is not root, it runs the container in a user namespace
//!   and mounts the image with the bundled `squashfuse_ll`, which reads every
//!   compression.
//! - For the real root (user 0 of the initial user namespace) with the
//!   capability `CAP_SYS_ADMIN`, it uses no user namespace and the kernel
//!   mounts the image. No setting of `apptainer.conf` changes this for root:
//!   the flag `--userns` is the one switch.
//!
//! Root of a user namespace, as in a rootless container, is not the real
//! root: apptainer mounts its images with `squashfuse_ll` too.
//!
//! A kernel mounts a squashfs only when it is built with the compression of
//! that squashfs (see [`kernel_squashfs`]). peppy packs each image with zstd
//! unless its node names another compression, and the WSL2 kernel of
//! Microsoft and the kernel of the NVIDIA Jetson are built without zstd for
//! squashfs. A container of the real root whose image the kernel cannot mount
//! stops with "kernel reported a bad superblock for squashfs image
//! partition".
//!
//! So a daemon that is the real root reads the compression of the image of
//! each `run` and each `exec` (see [`squashfs_image`]). For an image that
//! its kernel cannot mount it passes `--userns`, and apptainer mounts the
//! image with `squashfuse_ll` as it does for every other user. For every
//! other image, and for every daemon that is not the real root, peppy passes
//! no flag: a container of the real root whose image the kernel mounts keeps
//! the capabilities of the real root.
//!
//! The namespace maps user 0 and the real group of the daemon only. In it:
//!
//! - A container has no capability of the real root. It cannot, for example,
//!   take a real-time priority.
//! - Root has the rights of root only on a file of user 0 and of that group,
//!   and the permission bits decide for every other file. Apptainer enters
//!   its installation and the working directory of its process from the
//!   namespace, and finds each bind source from it. A command with one of
//!   these paths below a directory that those bits close to root (the home
//!   directory of another user, of mode 750) fails with an error that names
//!   that directory. Apptainer opens the image before it makes the namespace,
//!   so the image can be below such a directory.
//!
//! The permission bits do not always decide: a network file system and a
//! FUSE file system can apply rules of their own, and an entry of an access
//! control list can admit root where the bits do not. So peppy refuses a
//! command only for a directory that the kernel also reports closed (see
//! [`Host::root_enters_from_user_namespace`]), and the kernel alone decides
//! for a directory with a list. Without that answer peppy passes `--userns`,
//! and apptainer reports a path that it cannot reach.

mod host;
mod kernel_squashfs;
mod squashfs_image;

use super::facade::shell_escape_single_quoted;
use crate::error::{Error, Result};
use host::{DirectoryAccess, Host, ThisHost};
use kernel_squashfs::KernelSquashfs;
pub(crate) use squashfs_image::SquashfsCompression;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// The flag of `apptainer run` and `apptainer exec` that puts the container
/// in a user namespace.
const USER_NAMESPACE_FLAG: &str = "--userns";

/// The compression of the images that peppy builds: the one that
/// `SQUASHFS_COMPRESSION` of the facade names.
pub(crate) const COMPRESSION_OF_BUILT_IMAGES: SquashfsCompression = SquashfsCompression::Zstd;

/// How the user namespace of this process maps its users: one line of the
/// first user in the namespace, the first user outside it, and the count.
const UID_MAP_FILE: &str = "/proc/self/uid_map";

/// The fields of the one line of [`UID_MAP_FILE`] in the initial user
/// namespace: every user maps to itself.
const INITIAL_USER_NAMESPACE_MAP: [&str; 3] = ["0", "0", "4294967295"];

/// The user root.
const ROOT_USER: u32 = 0;

/// What peppy asks of apptainer for the user namespace of a `run` or an
/// `exec`.
#[derive(Debug)]
pub(crate) enum UserNamespace {
    /// peppy passes no flag: apptainer uses a user namespace for a user that
    /// is not root, and none for root.
    ApptainerDefault,
    /// The daemon is the real root: peppy passes [`USER_NAMESPACE_FLAG`] for
    /// an image that its kernel cannot mount.
    ForcedForUnmountableImages(RootDaemon),
}

impl UserNamespace {
    /// The choice for the daemon of this process on this host. It tells the
    /// operator, once for the process, what the user namespace changes for
    /// the containers of the images that peppy builds.
    pub(crate) fn for_this_daemon(apptainer_dir: &Path) -> Self {
        let user_namespace = Self::for_daemon(
            RealRoot::of_this_process(),
            KernelSquashfs::of_this_host().cloned(),
            Box::new(ThisHost),
        );
        if let Self::ForcedForUnmountableImages(daemon) = &user_namespace {
            static ANNOUNCED: Once = Once::new();
            ANNOUNCED.call_once(|| daemon.announce(apptainer_dir));
        }
        user_namespace
    }

    /// [`UserNamespace::for_this_daemon`] for a daemon and a host that the
    /// arguments describe: `real_root` is `None` for a daemon that is not the
    /// real root, and `kernel` is `None` for a kernel whose configuration
    /// cannot be read.
    ///
    /// Without a configuration to read, the kernel is taken to mount each
    /// image, as the kernel of each Ubuntu and Debian release mounts the
    /// images that peppy builds.
    fn for_daemon(
        real_root: Option<RealRoot>,
        kernel: Option<KernelSquashfs>,
        host: Box<dyn Host>,
    ) -> Self {
        let (Some(root), Some(kernel)) = (real_root, kernel) else {
            return Self::ApptainerDefault;
        };
        Self::ForcedForUnmountableImages(RootDaemon { root, kernel, host })
    }

    /// The flag that the `run` or the `exec` of `command` passes to
    /// apptainer, if any.
    ///
    /// # Errors
    ///
    /// When the command needs the user namespace and root cannot reach a
    /// path of the command from it: the container cannot start, and the
    /// error names the directory to open.
    pub(crate) fn flag_for(&self, command: &ImageCommand<'_>) -> Result<Option<&'static str>> {
        match self {
            Self::ApptainerDefault => Ok(None),
            Self::ForcedForUnmountableImages(daemon) => daemon.flag_for(command),
        }
    }
}

/// The paths of the host that a command which runs an image hands to
/// apptainer.
#[derive(Debug)]
pub(crate) struct ImageCommand<'a> {
    /// The image, as the command names it: a file, or a reference that is
    /// not a file (an image of a registry).
    pub(crate) image: &'a Path,
    /// The apptainer installation.
    pub(crate) apptainer_dir: &'a Path,
    /// The directory that the apptainer process runs in, or `None` when it
    /// runs in the working directory of the daemon.
    pub(crate) working_dir: Option<&'a Path>,
    /// The source of each bind mount.
    pub(crate) bind_sources: Vec<&'a Path>,
}

/// A daemon that is the real root, on a kernel whose configuration tells
/// the squashfs compressions that it mounts.
#[derive(Debug)]
pub(crate) struct RootDaemon {
    root: RealRoot,
    kernel: KernelSquashfs,
    host: Box<dyn Host>,
}

impl RootDaemon {
    /// [`UserNamespace::flag_for`] for this daemon.
    ///
    /// An image that tells no compression gets no flag: apptainer does not
    /// mount it as the squashfs of a SIF file, or reports that it cannot
    /// read it.
    fn flag_for(&self, command: &ImageCommand<'_>) -> Result<Option<&'static str>> {
        let Some(compression) = SquashfsCompression::of_image(command.image) else {
            return Ok(None);
        };
        if self.kernel.mounts(compression) {
            return Ok(None);
        }
        match self.first_path_out_of_reach(command) {
            Some(out_of_reach) => Err(out_of_reach.to_error(compression)),
            None => Ok(Some(USER_NAMESPACE_FLAG)),
        }
    }

    /// The first path of `command` that root cannot reach from the user
    /// namespace: the installation, then the working directory of the
    /// apptainer process, then each bind source.
    fn first_path_out_of_reach(&self, command: &ImageCommand<'_>) -> Option<PathOutOfReach> {
        let dir_of_the_daemon = match command.working_dir {
            Some(_) => None,
            None => self.host.current_dir(),
        };
        let installation = [(HostPathRole::Installation, command.apptainer_dir)];
        let working_dir = command
            .working_dir
            .or(dir_of_the_daemon.as_deref())
            .map(|dir| (HostPathRole::WorkingDirectory, dir));
        let bind_sources = command
            .bind_sources
            .iter()
            .map(|source| (HostPathRole::BindSource, *source));
        let mut kernel = self.kernel_answers();
        installation
            .into_iter()
            .chain(working_dir)
            .chain(bind_sources)
            .find_map(|(role, path)| self.out_of_reach(role, path, &mut kernel))
    }

    /// The answers of the kernel about this root, for the paths of one
    /// command.
    fn kernel_answers(&self) -> KernelAnswers<'_> {
        KernelAnswers {
            host: self.host.as_ref(),
            mapped_group: self.root.mapped_group,
            answers: HashMap::new(),
        }
    }

    /// The outermost directory that root cannot enter from the user
    /// namespace on the way to `path`, or `None` when root reaches the path.
    ///
    /// A directory closes the path only when its permissions can keep root
    /// out (see [`RealRoot::obstacle_in`]) and `kernel` reports it closed.
    /// A path that cannot be resolved and a directory whose permissions
    /// cannot be read close nothing: apptainer reports a path that it cannot
    /// reach.
    fn out_of_reach(
        &self,
        role: HostPathRole,
        path: &Path,
        kernel: &mut KernelAnswers<'_>,
    ) -> Option<PathOutOfReach> {
        // The kernel resolves symbolic links, so the search reads the
        // directories of the resolved path.
        let resolved_path = self.host.resolved(path)?;
        let innermost_dir = match role.reach() {
            Reach::Entered => resolved_path.as_path(),
            Reach::Found => resolved_path.parent()?,
        };
        // From the root of the file system down, so that the result is the
        // outermost closed directory: the first one to open.
        let mut dirs: Vec<&Path> = innermost_dir.ancestors().collect();
        dirs.reverse();
        let (closed_dir, obstacle) = dirs.into_iter().find_map(|dir| {
            let obstacle = self.root.obstacle_in(self.host.access_of(dir)?)?;
            kernel.reports_closed(dir).then_some((dir, obstacle))
        })?;
        Some(PathOutOfReach {
            role,
            path: resolved_path.clone(),
            closed_dir: closed_dir.to_path_buf(),
            obstacle,
        })
    }

    /// Tells the operator what the user namespace changes for the containers
    /// of the images that peppy builds, when the kernel cannot mount them.
    fn announce(&self, apptainer_dir: &Path) {
        if self.kernel.mounts(COMPRESSION_OF_BUILT_IMAGES) {
            return;
        }
        let installation_out_of_reach = self.out_of_reach(
            HostPathRole::Installation,
            apptainer_dir,
            &mut self.kernel_answers(),
        );
        let Some(out_of_reach) = installation_out_of_reach else {
            tracing::info!(
                "This daemon runs as root and its kernel cannot mount {} squashfs, the \
                 compression of the images that peppy builds, so apptainer runs their \
                 containers in a user namespace ({USER_NAMESPACE_FLAG}): such a node has no \
                 capability of the real root",
                COMPRESSION_OF_BUILT_IMAGES.name()
            );
            return;
        };
        tracing::warn!(
            "No container of an image that peppy builds can start. {}",
            out_of_reach.to_error(COMPRESSION_OF_BUILT_IMAGES)
        );
    }
}

/// What a path of the host is for a command that runs an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPathRole {
    Installation,
    WorkingDirectory,
    BindSource,
}

impl HostPathRole {
    /// The words that name a path of this role in an error.
    fn description(self) -> &'static str {
        match self {
            Self::Installation => "the apptainer installation",
            Self::WorkingDirectory => "the working directory of apptainer",
            Self::BindSource => "the bind source",
        }
    }

    /// How apptainer reaches a path of this role from the user namespace.
    fn reach(self) -> Reach {
        match self {
            Self::Installation | Self::WorkingDirectory => Reach::Entered,
            Self::BindSource => Reach::Found,
        }
    }
}

/// How apptainer reaches a path from the user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// It enters the path, a directory: root must enter the directory and
    /// each directory above it.
    Entered,
    /// It finds the path, a file or a directory: root must enter each
    /// directory above it.
    Found,
}

/// The answers of the kernel about one root, for the paths of one command.
/// Several paths of a command are below the same directories, so the kernel
/// answers one time for each directory.
struct KernelAnswers<'a> {
    host: &'a dyn Host,
    mapped_group: u32,
    answers: HashMap<PathBuf, Option<bool>>,
}

impl KernelAnswers<'_> {
    /// Whether the kernel reports that root cannot enter the directory at
    /// `dir` from the user namespace. No answer is not a report.
    fn reports_closed(&mut self, dir: &Path) -> bool {
        let answer = self.answers.entry(dir.to_path_buf()).or_insert_with(|| {
            self.host
                .root_enters_from_user_namespace(dir, self.mapped_group)
        });
        *answer == Some(false)
    }
}

/// A path of a command that root cannot reach from the user namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathOutOfReach {
    role: HostPathRole,
    /// The path, with its symbolic links resolved.
    path: PathBuf,
    /// The outermost directory that root cannot enter: the first one to
    /// open.
    closed_dir: PathBuf,
    /// What keeps root out of `closed_dir`.
    obstacle: Obstacle,
}

impl PathOutOfReach {
    /// The error of a command that runs an image packed with `compression`.
    fn to_error(&self, compression: SquashfsCompression) -> Error {
        Error::ClosedToRootInUserNamespace {
            compression: compression.name(),
            role: self.role.description(),
            path: self.path.display().to_string(),
            closed_dir: self.closed_dir.display().to_string(),
            open_command: self.obstacle.command_that_opens(&self.closed_dir),
        }
    }
}

/// A daemon that is the real root: user 0 of the initial user namespace. Its
/// groups decide which directories it enters from the user namespace of
/// apptainer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RealRoot {
    /// The real group: the one group that apptainer maps into the user
    /// namespace.
    mapped_group: u32,
    /// The supplementary groups. The user namespace maps none of them, and
    /// the kernel still reads the group bits of their directories for root.
    supplementary_groups: Vec<u32>,
}

impl RealRoot {
    /// The daemon of this process, or `None` when it is not the real root.
    /// Apptainer reads the real user and the real group of its process, so
    /// this reads the same two. A process whose user namespace or groups
    /// cannot be read is taken not to be the real root, so that peppy passes
    /// no flag for it.
    fn of_this_process() -> Option<Self> {
        if !rustix::process::getuid().is_root() {
            return None;
        }
        let uid_map = fs::read_to_string(UID_MAP_FILE).ok()?;
        if !is_initial_user_namespace_map(&uid_map) {
            return None;
        }
        let supplementary_groups = rustix::process::getgroups().ok()?;
        Some(Self {
            mapped_group: rustix::process::getgid().as_raw(),
            supplementary_groups: supplementary_groups
                .into_iter()
                .map(rustix::process::Gid::as_raw)
                .collect(),
        })
    }

    /// Whether `group` is one of the groups of this root.
    fn is_in_group(&self, group: u32) -> bool {
        group == self.mapped_group || self.supplementary_groups.contains(&group)
    }

    /// The class of permission bits that the kernel reads for this root on
    /// `dir`: the bits of the owner for a directory of root, the bits of the
    /// group for a directory of one of its groups, and the bits of the other
    /// users for every other directory.
    fn permission_class_on(&self, dir: DirectoryAccess) -> PermissionClass {
        if dir.owner == ROOT_USER {
            PermissionClass::Owner
        } else if self.is_in_group(dir.group) {
            PermissionClass::Group
        } else {
            PermissionClass::Others
        }
    }

    /// Whether this root has its capabilities on `dir` in the user namespace
    /// of apptainer, which maps user 0 and the real group only.
    ///
    /// Outside a user namespace, the capability `CAP_DAC_READ_SEARCH` lets
    /// root enter a directory whatever its permissions. In the namespace the
    /// capability holds only for a directory whose owner and group are both
    /// mapped.
    fn has_capabilities_on(&self, dir: DirectoryAccess) -> bool {
        dir.owner == ROOT_USER && dir.group == self.mapped_group
    }

    /// Whether the owner, the group and the permission bits of `dir` let
    /// this root enter it from the user namespace: by its capabilities, or
    /// by the bits of [`permission_class_on`](Self::permission_class_on),
    /// which decide for a directory without them.
    fn enters_from_user_namespace(&self, dir: DirectoryAccess) -> bool {
        let bits_admit_root = dir.mode & self.permission_class_on(dir).search_bit() != 0;
        self.has_capabilities_on(dir) || bits_admit_root
    }

    /// What can keep this root out of `dir` in the user namespace, or `None`
    /// when the permissions of `dir` let it enter.
    ///
    /// An access control list comes first: for a directory that has one, the
    /// kernel reads the entries of the list where this module reads the
    /// permission bits, so the bits tell nothing.
    fn obstacle_in(&self, dir: DirectoryAccess) -> Option<Obstacle> {
        if self.has_capabilities_on(dir) {
            return None;
        }
        if dir.has_access_control_list {
            return Some(Obstacle::AccessControlList);
        }
        (!self.enters_from_user_namespace(dir))
            .then(|| Obstacle::PermissionBits(self.permission_class_on(dir)))
    }
}

/// Whether `uid_map`, the text of a [`UID_MAP_FILE`], is the map of the
/// initial user namespace.
fn is_initial_user_namespace_map(uid_map: &str) -> bool {
    uid_map.split_whitespace().eq(INITIAL_USER_NAMESPACE_MAP)
}

/// What keeps root out of a directory in the user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Obstacle {
    /// The permission bits of the class that the kernel reads for root.
    PermissionBits(PermissionClass),
    /// The entries of the access control list of the directory.
    AccessControlList,
}

impl Obstacle {
    /// The shell command that lets root enter the directory at `dir`.
    fn command_that_opens(self, dir: &Path) -> String {
        let quoted_dir = shell_escape_single_quoted(&dir.to_string_lossy());
        match self {
            Self::PermissionBits(class) => {
                format!("chmod {}+x '{quoted_dir}'", class.chmod_letter())
            }
            Self::AccessControlList => format!("setfacl -m u:root:x '{quoted_dir}'"),
        }
    }
}

/// The users that one group of three permission bits of a file is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionClass {
    Owner,
    Group,
    Others,
}

impl PermissionClass {
    /// The bit of this class that lets a user enter a directory.
    fn search_bit(self) -> u32 {
        match self {
            Self::Owner => 0o100,
            Self::Group => 0o010,
            Self::Others => 0o001,
        }
    }

    /// The letter that names this class in a `chmod` command.
    fn chmod_letter(self) -> char {
        match self {
            Self::Owner => 'u',
            Self::Group => 'g',
            Self::Others => 'o',
        }
    }
}

/// Root daemons and hosts for tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::host::{DirectoryAccess, Host};
    use super::kernel_squashfs::fixtures::{CONFIG_WITHOUT_ZSTD, kernel_with_config};
    pub(crate) use super::squashfs_image::fixtures::sif_packed_with;
    use super::{RealRoot, UserNamespace};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    /// The group root.
    pub(crate) const ROOT_GROUP: u32 = 0;

    /// A user of the host that is not root.
    pub(crate) const OTHER_USER: u32 = 1000;

    /// The group of [`OTHER_USER`].
    pub(crate) const OTHER_GROUP: u32 = 1000;

    /// A directory that every user can enter.
    pub(crate) const OPEN_DIR: DirectoryAccess = DirectoryAccess {
        owner: OTHER_USER,
        group: OTHER_GROUP,
        mode: 0o755,
        has_access_control_list: false,
    };

    /// The home directory of a user of Ubuntu 24.04: closed to the other
    /// users.
    pub(crate) const PRIVATE_HOME: DirectoryAccess = DirectoryAccess {
        owner: OTHER_USER,
        group: OTHER_GROUP,
        mode: 0o750,
        has_access_control_list: false,
    };

    /// [`PRIVATE_HOME`] with an access control list.
    pub(crate) const PRIVATE_HOME_WITH_A_LIST: DirectoryAccess = DirectoryAccess {
        has_access_control_list: true,
        ..PRIVATE_HOME
    };

    /// Root as a system service runs it: in the group root and in no other
    /// group.
    pub(super) fn root_of_the_root_group() -> RealRoot {
        RealRoot {
            mapped_group: ROOT_GROUP,
            supplementary_groups: Vec::new(),
        }
    }

    /// A host that a test describes. Each path resolves to itself, each
    /// directory is [`OPEN_DIR`], and the kernel reports closed each
    /// directory that it is asked about, unless the test says otherwise.
    #[derive(Debug, Default)]
    pub(crate) struct FakeHost {
        dirs: HashMap<PathBuf, Option<DirectoryAccess>>,
        kernel_answers: HashMap<PathBuf, Option<bool>>,
        kernel_questions: KernelQuestions,
        unresolved_paths: Vec<PathBuf>,
        current_dir: Option<PathBuf>,
    }

    /// The directories that the kernel of a [`FakeHost`] was asked about, in
    /// the order of the questions.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct KernelQuestions(Arc<Mutex<Vec<PathBuf>>>);

    impl KernelQuestions {
        fn record(&self, dir: &Path) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(dir.to_path_buf());
        }

        /// The directories of the questions so far.
        pub(crate) fn asked(&self) -> Vec<PathBuf> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl FakeHost {
        /// With the permissions `access` on the directory at `dir`.
        pub(crate) fn with_dir(mut self, dir: &str, access: DirectoryAccess) -> Self {
            self.dirs.insert(PathBuf::from(dir), Some(access));
            self
        }

        /// With a directory at `dir` whose permissions cannot be read.
        pub(crate) fn with_unreadable_dir(mut self, dir: &str) -> Self {
            self.dirs.insert(PathBuf::from(dir), None);
            self
        }

        /// The questions that the kernel of this host gets, which a test
        /// reads after it hands the host to a daemon.
        pub(crate) fn kernel_questions(&self) -> KernelQuestions {
            self.kernel_questions.clone()
        }

        /// With `answer` as what the kernel tells about root in the
        /// directory at `dir`.
        pub(crate) fn with_kernel_answer(mut self, dir: &str, answer: Option<bool>) -> Self {
            self.kernel_answers.insert(PathBuf::from(dir), answer);
            self
        }

        /// With a path at `path` that cannot be resolved.
        pub(crate) fn with_unresolved_path(mut self, path: &str) -> Self {
            self.unresolved_paths.push(PathBuf::from(path));
            self
        }

        /// With `dir` as the working directory of the daemon.
        pub(crate) fn with_current_dir(mut self, dir: &str) -> Self {
            self.current_dir = Some(PathBuf::from(dir));
            self
        }
    }

    impl Host for FakeHost {
        fn resolved(&self, path: &Path) -> Option<PathBuf> {
            let is_unresolved = self
                .unresolved_paths
                .iter()
                .any(|unresolved| unresolved == path);
            (!is_unresolved).then(|| path.to_path_buf())
        }

        fn access_of(&self, dir: &Path) -> Option<DirectoryAccess> {
            self.dirs.get(dir).copied().unwrap_or(Some(OPEN_DIR))
        }

        fn root_enters_from_user_namespace(&self, dir: &Path, _mapped_group: u32) -> Option<bool> {
            self.kernel_questions.record(dir);
            self.kernel_answers.get(dir).copied().unwrap_or(Some(false))
        }

        fn current_dir(&self) -> Option<PathBuf> {
            self.current_dir.clone()
        }
    }

    /// The choice of a daemon that is `root` on `host`, with the kernel of
    /// the Jetson and of WSL2: it mounts gzip and xz squashfs, and no zstd
    /// squashfs.
    pub(super) fn real_root_on_a_kernel_without_zstd(
        root: RealRoot,
        host: FakeHost,
    ) -> UserNamespace {
        UserNamespace::for_daemon(
            Some(root),
            Some(kernel_with_config(CONFIG_WITHOUT_ZSTD)),
            Box::new(host),
        )
    }

    /// [`real_root_on_a_kernel_without_zstd`] for the root of a system
    /// service.
    pub(crate) fn root_daemon_on_a_kernel_without_zstd(host: FakeHost) -> UserNamespace {
        real_root_on_a_kernel_without_zstd(root_of_the_root_group(), host)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{
        FakeHost, OPEN_DIR, OTHER_GROUP, OTHER_USER, PRIVATE_HOME, PRIVATE_HOME_WITH_A_LIST,
        ROOT_GROUP, real_root_on_a_kernel_without_zstd, root_daemon_on_a_kernel_without_zstd,
        root_of_the_root_group, sif_packed_with,
    };
    use super::kernel_squashfs::fixtures::{
        CONFIG_WITH_ZSTD, CONFIG_WITHOUT_ZSTD, kernel_with_config,
    };
    use super::*;
    use crate::apptainer::scratch_dir;
    use std::assert_matches;
    use tempfile::TempDir;

    /// The installation of the commands of these tests.
    const INSTALLATION: &str = "/opt/peppy/bin/apptainer";

    /// An image file packed with `compression`, in a scratch directory that
    /// lives as long as the returned guard.
    fn image_packed_with(compression: SquashfsCompression) -> (TempDir, PathBuf) {
        let scratch = scratch_dir();
        let image = scratch.path().join("node.sif");
        fs::write(&image, sif_packed_with(compression)).expect("write the image");
        (scratch, image)
    }

    /// A command that runs `image` from [`INSTALLATION`], in the working
    /// directory `/srv/node`, with no bind mount.
    fn command_running(image: &Path) -> ImageCommand<'_> {
        ImageCommand {
            image,
            apptainer_dir: Path::new(INSTALLATION),
            working_dir: Some(Path::new("/srv/node")),
            bind_sources: Vec::new(),
        }
    }

    /// The flag for a command that runs a zstd image, for the root of a
    /// system service on `host` with a kernel without zstd squashfs.
    fn flag_for_a_zstd_image(
        host: FakeHost,
        change_command: impl FnOnce(&mut ImageCommand<'_>),
    ) -> Result<Option<&'static str>> {
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);
        let mut command = command_running(&image);
        change_command(&mut command);
        root_daemon_on_a_kernel_without_zstd(host).flag_for(&command)
    }

    /// The path that root cannot reach, from the error of a refused command.
    fn refused(flag: Result<Option<&'static str>>) -> (String, String, String) {
        match flag.expect_err("the command is refused") {
            Error::ClosedToRootInUserNamespace {
                role,
                path,
                closed_dir,
                ..
            } => (role.to_string(), path, closed_dir),
            other => panic!("the error names a closed directory, got: {other}"),
        }
    }

    #[test]
    fn the_map_of_every_user_to_itself_is_the_initial_user_namespace() {
        assert!(is_initial_user_namespace_map(
            "         0          0 4294967295\n"
        ));
    }

    #[test]
    fn the_map_of_a_container_is_not_the_initial_user_namespace() {
        let root_of_apptainer = "         0          0          1\n";
        let root_of_a_rootless_container = "         0       1000          1\n";
        let rootless_container_with_subordinate_users =
            "         0       1000          1\n         1     100000      65536\n";

        assert!(!is_initial_user_namespace_map(root_of_apptainer));
        assert!(!is_initial_user_namespace_map(root_of_a_rootless_container));
        assert!(!is_initial_user_namespace_map(
            rootless_container_with_subordinate_users
        ));
        assert!(!is_initial_user_namespace_map(""));
    }

    #[test]
    fn the_real_root_forces_the_user_namespace_for_an_image_that_its_kernel_cannot_mount() {
        let flag = flag_for_a_zstd_image(FakeHost::default(), |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// The kernel of the Jetson and of WSL2 mounts gzip and xz squashfs, so
    /// a container of such an image keeps the capabilities of the real root.
    #[test]
    fn the_real_root_passes_no_flag_for_an_image_that_its_kernel_mounts() {
        let user_namespace = root_daemon_on_a_kernel_without_zstd(FakeHost::default());

        for compression in [SquashfsCompression::Gzip, SquashfsCompression::Xz] {
            let (_scratch, image) = image_packed_with(compression);

            let flag = user_namespace.flag_for(&command_running(&image));

            assert_matches!(
                flag,
                Ok(None),
                "an image packed with {}",
                compression.name()
            );
        }
    }

    #[test]
    fn the_real_root_on_a_kernel_with_zstd_squashfs_passes_no_flag_for_a_zstd_image() {
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);
        let user_namespace = UserNamespace::for_daemon(
            Some(root_of_the_root_group()),
            Some(kernel_with_config(CONFIG_WITH_ZSTD)),
            Box::new(FakeHost::default()),
        );

        let flag = user_namespace.flag_for(&command_running(&image));

        assert_matches!(flag, Ok(None));
    }

    #[test]
    fn an_image_that_tells_no_compression_gets_no_flag() {
        let scratch = scratch_dir();
        let user_namespace = root_daemon_on_a_kernel_without_zstd(FakeHost::default());

        let of_a_registry = user_namespace.flag_for(&command_running(Path::new("docker://alpine")));
        let sandbox = user_namespace.flag_for(&command_running(scratch.path()));
        let missing = scratch.path().join("no-such-image.sif");
        let missing = user_namespace.flag_for(&command_running(&missing));

        assert_matches!(of_a_registry, Ok(None));
        assert_matches!(sandbox, Ok(None));
        assert_matches!(missing, Ok(None));
    }

    /// The reach of a path matters only in the user namespace, so a closed
    /// directory does not stop a container whose image the kernel mounts.
    #[test]
    fn an_image_that_the_kernel_mounts_runs_from_an_installation_out_of_reach() {
        let (_scratch, image) = image_packed_with(SquashfsCompression::Gzip);
        let host = FakeHost::default().with_dir("/opt/peppy", PRIVATE_HOME);
        let user_namespace = root_daemon_on_a_kernel_without_zstd(host);

        let flag = user_namespace.flag_for(&command_running(&image));

        assert_matches!(flag, Ok(None));
    }

    #[test]
    fn a_daemon_that_is_not_the_real_root_keeps_the_default_of_apptainer() {
        let user_namespace = UserNamespace::for_daemon(
            None,
            Some(kernel_with_config(CONFIG_WITHOUT_ZSTD)),
            Box::new(FakeHost::default()),
        );

        assert_matches!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn the_real_root_with_no_kernel_configuration_keeps_the_default_of_apptainer() {
        let user_namespace = UserNamespace::for_daemon(
            Some(root_of_the_root_group()),
            None,
            Box::new(FakeHost::default()),
        );

        assert_matches!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn the_default_of_apptainer_passes_no_flag_and_reads_no_image() {
        let missing_image = Path::new("/no/such/image.sif");

        let flag = UserNamespace::ApptainerDefault.flag_for(&command_running(missing_image));

        assert_matches!(flag, Ok(None));
    }

    #[test]
    fn root_enters_a_directory_of_root_and_of_its_real_group_whatever_its_bits() {
        let closed_to_all = DirectoryAccess {
            owner: ROOT_USER,
            group: ROOT_GROUP,
            mode: 0o000,
            has_access_control_list: false,
        };

        assert!(root_of_the_root_group().enters_from_user_namespace(closed_to_all));
    }

    #[test]
    fn root_enters_a_directory_of_another_user_by_the_bits_of_the_other_users() {
        let root = root_of_the_root_group();

        assert!(root.enters_from_user_namespace(OPEN_DIR));
        assert!(!root.enters_from_user_namespace(PRIVATE_HOME));
    }

    #[test]
    fn root_enters_a_directory_of_its_real_group_by_the_bits_of_the_group() {
        let root = root_of_the_root_group();
        let open_to_group = DirectoryAccess {
            owner: OTHER_USER,
            group: ROOT_GROUP,
            mode: 0o750,
            has_access_control_list: false,
        };
        let open_to_others_only = DirectoryAccess {
            owner: OTHER_USER,
            group: ROOT_GROUP,
            mode: 0o705,
            has_access_control_list: false,
        };

        assert!(root.enters_from_user_namespace(open_to_group));
        assert!(!root.enters_from_user_namespace(open_to_others_only));
    }

    #[test]
    fn root_enters_a_directory_of_a_supplementary_group_by_the_bits_of_the_group() {
        let root = RealRoot {
            mapped_group: ROOT_GROUP,
            supplementary_groups: vec![OTHER_GROUP],
        };
        let closed_to_group = DirectoryAccess {
            mode: 0o700,
            ..PRIVATE_HOME
        };

        assert!(root.enters_from_user_namespace(PRIVATE_HOME));
        assert!(!root.enters_from_user_namespace(closed_to_group));
    }

    #[test]
    fn root_enters_its_own_directory_of_another_group_by_the_bits_of_the_owner() {
        let root = root_of_the_root_group();
        let open_to_owner = DirectoryAccess {
            owner: ROOT_USER,
            group: OTHER_GROUP,
            mode: 0o700,
            has_access_control_list: false,
        };
        let closed_to_owner = DirectoryAccess {
            owner: ROOT_USER,
            group: OTHER_GROUP,
            mode: 0o077,
            has_access_control_list: false,
        };

        assert!(root.enters_from_user_namespace(open_to_owner));
        assert!(!root.enters_from_user_namespace(closed_to_owner));
    }

    /// The user namespace maps the real group of the daemon, so the group
    /// root is not in it for a root of another real group.
    #[test]
    fn root_of_another_real_group_has_the_rights_of_root_on_that_group_only() {
        let root = RealRoot {
            mapped_group: OTHER_GROUP,
            supplementary_groups: Vec::new(),
        };
        let of_root_and_its_real_group = DirectoryAccess {
            owner: ROOT_USER,
            group: OTHER_GROUP,
            mode: 0o000,
            has_access_control_list: false,
        };
        let of_root_and_the_root_group = DirectoryAccess {
            owner: ROOT_USER,
            group: ROOT_GROUP,
            mode: 0o000,
            has_access_control_list: false,
        };

        assert!(root.enters_from_user_namespace(of_root_and_its_real_group));
        assert!(!root.enters_from_user_namespace(of_root_and_the_root_group));
    }

    #[test]
    fn an_installation_below_a_closed_directory_is_refused_for_the_outermost_one() {
        let host = FakeHost::default()
            .with_dir("/home/user", PRIVATE_HOME)
            .with_dir("/home/user/.peppy", PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |command| {
            command.apptainer_dir = Path::new("/home/user/.peppy/bin/apptainer");
        });

        assert_eq!(
            refused(flag),
            (
                "the apptainer installation".to_string(),
                "/home/user/.peppy/bin/apptainer".to_string(),
                "/home/user".to_string()
            )
        );
    }

    #[test]
    fn an_installation_that_is_itself_closed_is_refused() {
        let host = FakeHost::default().with_dir(INSTALLATION, PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |_| {});

        let (_, _, closed_dir) = refused(flag);
        assert_eq!(closed_dir, INSTALLATION);
    }

    #[test]
    fn a_working_directory_below_a_closed_directory_is_refused() {
        let host = FakeHost::default().with_dir("/home/user", PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |command| {
            command.working_dir = Some(Path::new("/home/user/node"));
        });

        assert_eq!(
            refused(flag),
            (
                "the working directory of apptainer".to_string(),
                "/home/user/node".to_string(),
                "/home/user".to_string()
            )
        );
    }

    /// Apptainer runs in the working directory of the daemon when its
    /// command names none.
    #[test]
    fn a_command_without_a_working_directory_is_refused_for_the_one_of_the_daemon() {
        let host = FakeHost::default()
            .with_dir("/home/user", PRIVATE_HOME)
            .with_current_dir("/home/user/started-here");

        let flag = flag_for_a_zstd_image(host, |command| command.working_dir = None);

        assert_eq!(
            refused(flag),
            (
                "the working directory of apptainer".to_string(),
                "/home/user/started-here".to_string(),
                "/home/user".to_string()
            )
        );
    }

    #[test]
    fn the_working_directory_of_the_daemon_is_not_read_for_a_command_that_names_one() {
        let host = FakeHost::default()
            .with_dir("/home/user", PRIVATE_HOME)
            .with_current_dir("/home/user/started-here");

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    #[test]
    fn a_daemon_whose_working_directory_cannot_be_read_forces_the_user_namespace() {
        let flag = flag_for_a_zstd_image(FakeHost::default(), |command| command.working_dir = None);

        assert_matches!(flag, Ok(Some("--userns")));
    }

    #[test]
    fn a_bind_source_below_a_closed_directory_is_refused() {
        let host = FakeHost::default().with_dir("/home/user", PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |command| {
            command.bind_sources = vec![Path::new("/srv/data"), Path::new("/home/user/data")];
        });

        assert_eq!(
            refused(flag),
            (
                "the bind source".to_string(),
                "/home/user/data".to_string(),
                "/home/user".to_string()
            )
        );
    }

    /// Apptainer finds a bind source and does not enter it, so the
    /// permissions of the source itself close nothing.
    #[test]
    fn a_bind_source_that_is_itself_closed_is_in_reach() {
        let host = FakeHost::default().with_dir("/srv/private-data", PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |command| {
            command.bind_sources = vec![Path::new("/srv/private-data")];
        });

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// Apptainer opens the image before it makes the user namespace.
    #[test]
    fn an_image_below_a_closed_directory_is_in_reach() {
        let (scratch, image) = image_packed_with(SquashfsCompression::Zstd);
        let scratch_dir = scratch.path().to_string_lossy().into_owned();
        let host = FakeHost::default().with_dir(&scratch_dir, PRIVATE_HOME);
        let user_namespace = root_daemon_on_a_kernel_without_zstd(host);

        let flag = user_namespace.flag_for(&command_running(&image));

        assert_matches!(flag, Ok(Some("--userns")));
    }

    #[test]
    fn a_root_in_the_group_of_a_closed_directory_reaches_the_paths_below_it() {
        let (_scratch, image) = image_packed_with(SquashfsCompression::Zstd);
        let root = RealRoot {
            mapped_group: ROOT_GROUP,
            supplementary_groups: vec![OTHER_GROUP],
        };
        let host = FakeHost::default().with_dir("/opt/peppy", PRIVATE_HOME);
        let user_namespace = real_root_on_a_kernel_without_zstd(root, host);

        let flag = user_namespace.flag_for(&command_running(&image));

        assert_matches!(flag, Ok(Some("--userns")));
    }

    #[test]
    fn a_closed_directory_of_a_group_of_root_is_opened_for_the_group() {
        let closed_to_its_group = DirectoryAccess {
            owner: OTHER_USER,
            group: ROOT_GROUP,
            mode: 0o700,
            has_access_control_list: false,
        };
        let host = FakeHost::default().with_dir("/opt/peppy", closed_to_its_group);

        let error = flag_for_a_zstd_image(host, |_| {}).expect_err("the command is refused");

        assert!(
            error.to_string().contains("chmod g+x '/opt/peppy'\n"),
            "the command opens the directory for the group: {error}"
        );
    }

    #[test]
    fn a_directory_whose_permissions_cannot_be_read_closes_nothing() {
        let all_unreadable = FakeHost::default()
            .with_unreadable_dir("/opt")
            .with_unreadable_dir("/opt/peppy");
        let closed_below_an_unreadable_one = FakeHost::default()
            .with_unreadable_dir("/opt")
            .with_dir("/opt/peppy", PRIVATE_HOME);

        let forced = flag_for_a_zstd_image(all_unreadable, |_| {});
        let refused_below = flag_for_a_zstd_image(closed_below_an_unreadable_one, |_| {});

        assert_matches!(forced, Ok(Some("--userns")));
        let (_, _, closed_dir) = refused(refused_below);
        assert_eq!(closed_dir, "/opt/peppy");
    }

    /// An entry of the list can admit root where the permission bits do
    /// not, so the kernel alone decides for a directory with a list.
    #[test]
    fn a_directory_with_a_list_that_the_kernel_lets_root_enter_closes_nothing() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME_WITH_A_LIST)
            .with_kernel_answer("/opt/peppy", Some(true));

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// The list of a directory can also keep root out where its permission
    /// bits let every user enter.
    #[test]
    fn a_directory_with_a_list_that_the_kernel_reports_closed_is_opened_with_a_list_entry() {
        let open_by_its_bits = DirectoryAccess {
            has_access_control_list: true,
            ..OPEN_DIR
        };
        for with_a_list in [PRIVATE_HOME_WITH_A_LIST, open_by_its_bits] {
            let host = FakeHost::default().with_dir("/opt/peppy", with_a_list);

            let error = flag_for_a_zstd_image(host, |_| {}).expect_err("the command is refused");

            assert!(
                error
                    .to_string()
                    .contains("setfacl -m u:root:x '/opt/peppy'\n"),
                "the command adds an entry for root to the list: {error}"
            );
        }
    }

    #[test]
    fn a_directory_with_a_list_that_the_kernel_gives_no_answer_about_closes_nothing() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME_WITH_A_LIST)
            .with_kernel_answer("/opt/peppy", None);

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// Root has its capabilities on a directory of user 0 and of its real
    /// group, whatever the list of the directory holds.
    #[test]
    fn a_directory_of_root_with_a_list_is_not_a_question_for_the_kernel() {
        let of_root_with_a_list = DirectoryAccess {
            owner: ROOT_USER,
            group: ROOT_GROUP,
            mode: 0o700,
            has_access_control_list: true,
        };
        let host = FakeHost::default().with_dir("/opt/peppy", of_root_with_a_list);
        let questions = host.kernel_questions();

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
        assert!(questions.asked().is_empty());
    }

    /// Several paths of a command are below the same directory.
    #[test]
    fn the_kernel_answers_one_time_for_a_directory_that_several_paths_are_below() {
        let host = FakeHost::default()
            .with_dir("/home/user", PRIVATE_HOME_WITH_A_LIST)
            .with_kernel_answer("/home/user", Some(true));
        let questions = host.kernel_questions();

        let flag = flag_for_a_zstd_image(host, |command| {
            command.apptainer_dir = Path::new("/home/user/.peppy/bin/apptainer");
            command.working_dir = Some(Path::new("/home/user/.peppy/instances/node"));
            command.bind_sources = vec![
                Path::new("/home/user/.peppy/logs"),
                Path::new("/home/user/.peppy/conf"),
            ];
        });

        assert_matches!(flag, Ok(Some("--userns")));
        assert_eq!(questions.asked(), [Path::new("/home/user")]);
    }

    /// A host where each path is in reach starts no probe.
    #[test]
    fn the_kernel_gets_no_question_on_a_host_where_each_directory_is_open() {
        let host = FakeHost::default();
        let questions = host.kernel_questions();

        let flag = flag_for_a_zstd_image(host, |command| {
            command.bind_sources = vec![Path::new("/srv/data")];
        });

        assert_matches!(flag, Ok(Some("--userns")));
        assert!(questions.asked().is_empty());
    }

    #[test]
    fn a_path_that_cannot_be_resolved_closes_nothing() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME)
            .with_unresolved_path(INSTALLATION);

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// The permission bits do not decide on each file system, so the kernel
    /// has the last word on a directory that they close.
    #[test]
    fn a_directory_that_the_kernel_lets_root_enter_closes_nothing() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME)
            .with_kernel_answer("/opt/peppy", Some(true));

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    #[test]
    fn a_directory_that_the_kernel_gives_no_answer_about_closes_nothing() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME)
            .with_kernel_answer("/opt/peppy", None);

        let flag = flag_for_a_zstd_image(host, |_| {});

        assert_matches!(flag, Ok(Some("--userns")));
    }

    /// A path that the kernel does not report closed does not hide a later
    /// path that it does.
    #[test]
    fn a_later_path_is_refused_after_a_path_that_the_kernel_does_not_close() {
        let host = FakeHost::default()
            .with_dir("/opt/peppy", PRIVATE_HOME)
            .with_kernel_answer("/opt/peppy", Some(true))
            .with_dir("/home/user", PRIVATE_HOME);

        let flag = flag_for_a_zstd_image(host, |command| {
            command.bind_sources = vec![Path::new("/home/user/data")];
        });

        let (role, _, closed_dir) = refused(flag);
        assert_eq!(role, "the bind source");
        assert_eq!(closed_dir, "/home/user");
    }

    #[test]
    fn a_refused_command_gives_the_command_that_opens_the_directory() {
        let host = FakeHost::default().with_dir("/home/user", PRIVATE_HOME);

        let error = flag_for_a_zstd_image(host, |command| {
            command.apptainer_dir = Path::new("/home/user/.peppy/bin/apptainer");
        })
        .expect_err("the command is refused");

        let message = error.to_string();
        assert!(
            message.contains("cannot mount the zstd squashfs of the image"),
            "the message names the compression of the image: {message}"
        );
        assert!(
            message.contains("chmod o+x '/home/user'\n"),
            "the message gives the command that opens the directory: {message}"
        );
    }

    /// The operator copies the command, so a directory with a space or a
    /// quote in its name stays one argument of it.
    #[test]
    fn the_command_that_opens_a_directory_quotes_its_path() {
        let host = FakeHost::default().with_dir("/home/jean luc's", PRIVATE_HOME);

        let error = flag_for_a_zstd_image(host, |command| {
            command.apptainer_dir = Path::new("/home/jean luc's/.peppy/bin/apptainer");
        })
        .expect_err("the command is refused");

        let message = error.to_string();
        assert!(
            message.contains(r"chmod o+x '/home/jean luc'\''s'"),
            "the path is one quoted argument: {message}"
        );
    }
}
