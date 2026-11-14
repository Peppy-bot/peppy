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
//! peppy packs each image with zstd, and some kernels are built without zstd
//! for squashfs (`# CONFIG_SQUASHFS_ZSTD is not set`): the WSL2 kernel of
//! Microsoft and the kernel of the NVIDIA Jetson are two. On such a kernel a
//! container of a daemon that is the real root stops with "kernel reported a
//! bad superblock for squashfs image partition".
//!
//! So such a daemon on such a kernel passes `--userns` to each `run` and each
//! `exec` ([`UserNamespace::Forced`]), and apptainer mounts the image with
//! `squashfuse_ll` as it does for every other user. On every other host peppy
//! passes no flag ([`UserNamespace::ApptainerDefault`]), so a root daemon on
//! a kernel with zstd squashfs keeps the capabilities of the real root.
//!
//! The namespace maps user 0 and the effective group of the daemon only. In
//! it:
//!
//! - A container has no capability of the real root. It cannot, for example,
//!   take a real-time priority.
//! - Root has the rights of root only on a file of user 0 and of that group,
//!   and the permission bits decide for every other file. An apptainer
//!   installation below a directory that those bits close to root (the home
//!   directory of another user, of mode 750) is thus out of its reach
//!   ([`UserNamespace::OutOfReach`]): each `run` and each `exec` fails with
//!   an error that names that directory. The daemon itself, a build and a
//!   process node use no user namespace, so they work there.

use crate::error::{Error, Result};
use flate2::read::GzDecoder;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// The flag of `apptainer run` and `apptainer exec` that puts the container
/// in a user namespace.
const USER_NAMESPACE_FLAG: &str = "--userns";

/// The configuration of the running kernel, compressed with gzip. A kernel
/// built with `CONFIG_IKCONFIG_PROC` has it, and a container reads it too.
const PROC_KERNEL_CONFIG: &str = "/proc/config.gz";

/// The release of the running kernel, which names its configuration in
/// [`BOOT_DIR`].
const KERNEL_RELEASE_FILE: &str = "/proc/sys/kernel/osrelease";

/// Where a distribution keeps the configuration of each kernel it installs,
/// as `config-<release>`.
const BOOT_DIR: &str = "/boot";

/// The start of each line of a kernel configuration that sets an option.
const KERNEL_OPTION_PREFIX: &str = "CONFIG_";

/// The line of a kernel configuration that puts zstd into the squashfs
/// driver. The option is a boolean, so no other value turns it on.
const ZSTD_SQUASHFS_ENABLED: &str = "CONFIG_SQUASHFS_ZSTD=y";

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UserNamespace {
    /// peppy passes no flag: apptainer uses a user namespace for a user that
    /// is not root, and none for root.
    ApptainerDefault,
    /// peppy passes [`USER_NAMESPACE_FLAG`]: the daemon is the real root and
    /// its kernel cannot mount the zstd squashfs of an image.
    Forced,
    /// The daemon needs the user namespace as for [`UserNamespace::Forced`],
    /// and root cannot reach the apptainer installation from it: no `run`
    /// and no `exec` can start a container.
    OutOfReach(ClosedInstallDir),
}

impl UserNamespace {
    /// The choice for the daemon of this process on this host.
    ///
    /// It does not fail for [`UserNamespace::OutOfReach`]: the daemon, a
    /// build and a process node use no user namespace. The commands that run
    /// an image fail, through [`UserNamespace::flag`].
    pub(crate) fn for_this_daemon(apptainer_dir: &Path) -> Self {
        let user_namespace = Self::for_daemon(
            RealRoot::of_this_process().as_ref(),
            &KernelConfigFiles::of_this_host(),
            apptainer_dir,
            DirectoryAccess::of,
        );
        user_namespace.announce();
        user_namespace
    }

    /// [`UserNamespace::for_this_daemon`] for a daemon and a host that the
    /// arguments describe: `real_root` is `None` for a daemon that is not the
    /// real root, and `access_of` reads the owner, the group and the
    /// permission bits of a directory.
    fn for_daemon(
        real_root: Option<&RealRoot>,
        kernel_config_files: &KernelConfigFiles,
        apptainer_dir: &Path,
        access_of: impl Fn(&Path) -> io::Result<DirectoryAccess>,
    ) -> Self {
        let Some(real_root) = real_root else {
            return Self::ApptainerDefault;
        };
        // Without a configuration to read, the kernel is taken to mount zstd
        // squashfs, as the kernel of each Ubuntu and Debian release does.
        let Some(kernel_config) = kernel_config_files.read() else {
            return Self::ApptainerDefault;
        };
        if kernel_config.mounts_zstd_squashfs() {
            return Self::ApptainerDefault;
        }
        match real_root.closed_dir_on_the_way_to(apptainer_dir, access_of) {
            Some(closed_dir) => Self::OutOfReach(closed_dir),
            None => Self::Forced,
        }
    }

    /// The flag that a `run` or an `exec` passes to apptainer, if any.
    ///
    /// # Errors
    ///
    /// For [`UserNamespace::OutOfReach`]: the command cannot start a
    /// container, and the error names the directory to open.
    pub(crate) fn flag(&self) -> Result<Option<&'static str>> {
        match self {
            Self::ApptainerDefault => Ok(None),
            Self::Forced => Ok(Some(USER_NAMESPACE_FLAG)),
            Self::OutOfReach(closed_dir) => Err(closed_dir.to_error()),
        }
    }

    /// Tells the operator, once for the process and for each choice, what
    /// the user namespace changes for the containers of this root daemon.
    fn announce(&self) {
        static FORCED: Once = Once::new();
        static OUT_OF_REACH: Once = Once::new();
        match self {
            Self::ApptainerDefault => {}
            Self::Forced => FORCED.call_once(|| {
                tracing::info!(
                    "This daemon runs as root and the kernel cannot mount zstd squashfs, so \
                     apptainer runs each container in a user namespace ({USER_NAMESPACE_FLAG}): \
                     a node has no capability of the real root"
                );
            }),
            Self::OutOfReach(closed_dir) => OUT_OF_REACH.call_once(|| {
                tracing::warn!(
                    "No container of this daemon can start. {}",
                    closed_dir.to_error()
                );
            }),
        }
    }
}

/// A directory on the way to the apptainer installation that root cannot
/// enter from the user namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedInstallDir {
    /// The apptainer installation, with its symbolic links resolved.
    pub(crate) apptainer_dir: PathBuf,
    /// The outermost directory that root cannot enter: the first one to
    /// open.
    pub(crate) closed_dir: PathBuf,
    /// The permission bits of `closed_dir` that the kernel reads for root.
    pub(crate) class_of_root: PermissionClass,
}

impl ClosedInstallDir {
    /// The error of a command that runs an image.
    fn to_error(&self) -> Error {
        Error::InstallDirClosedToRootInUserNamespace {
            apptainer_dir: self.apptainer_dir.display().to_string(),
            closed_dir: self.closed_dir.display().to_string(),
            chmod_class: self.class_of_root.chmod_letter(),
        }
    }
}

/// A daemon that is the real root: user 0 of the initial user namespace. Its
/// groups decide which directories it enters from the user namespace of
/// apptainer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RealRoot {
    /// The effective group: the one group that apptainer maps into the user
    /// namespace.
    effective_group: u32,
    /// The supplementary groups. The user namespace maps none of them, and
    /// the kernel still reads the group bits of their directories for root.
    supplementary_groups: Vec<u32>,
}

impl RealRoot {
    /// The daemon of this process, or `None` when it is not the real root. A
    /// process whose user namespace or groups cannot be read is taken not to
    /// be, so that peppy passes no flag for it.
    fn of_this_process() -> Option<Self> {
        if !rustix::process::geteuid().is_root() {
            return None;
        }
        let uid_map = fs::read_to_string(UID_MAP_FILE).ok()?;
        if !is_initial_user_namespace_map(&uid_map) {
            return None;
        }
        let supplementary_groups = rustix::process::getgroups().ok()?;
        Some(Self {
            effective_group: rustix::process::getegid().as_raw(),
            supplementary_groups: supplementary_groups
                .into_iter()
                .map(rustix::process::Gid::as_raw)
                .collect(),
        })
    }

    /// Whether `group` is one of the groups of this root.
    fn is_in_group(&self, group: u32) -> bool {
        group == self.effective_group || self.supplementary_groups.contains(&group)
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

    /// Whether this root can enter `dir` from the user namespace of
    /// apptainer, which maps user 0 and the effective group only.
    ///
    /// Outside a user namespace, the capability `CAP_DAC_READ_SEARCH` lets
    /// root enter a directory whatever its permission bits. In the namespace
    /// the capability holds only for a directory whose owner and group are
    /// both mapped, so the bits of
    /// [`permission_class_on`](Self::permission_class_on) decide for every
    /// other directory.
    fn enters_from_user_namespace(&self, dir: DirectoryAccess) -> bool {
        let owner_and_group_are_mapped =
            dir.owner == ROOT_USER && dir.group == self.effective_group;
        let bits_admit_root = dir.mode & self.permission_class_on(dir).search_bit() != 0;
        owner_and_group_are_mapped || bits_admit_root
    }

    /// The outermost directory, from the root of the file system down to
    /// `apptainer_dir`, that this root cannot enter from the user namespace,
    /// or `None` when it reaches the installation.
    ///
    /// An installation that cannot be resolved and a directory whose
    /// permissions cannot be read decide nothing: apptainer reports a path
    /// that it cannot reach.
    fn closed_dir_on_the_way_to(
        &self,
        apptainer_dir: &Path,
        access_of: impl Fn(&Path) -> io::Result<DirectoryAccess>,
    ) -> Option<ClosedInstallDir> {
        // The kernel resolves symbolic links, so the search reads the
        // directories of the resolved path.
        let resolved_dir = apptainer_dir.canonicalize().ok()?;
        // From the root of the file system down, so that the result is the
        // outermost closed directory: the first one to open.
        let mut dirs: Vec<&Path> = resolved_dir.ancestors().collect();
        dirs.reverse();
        dirs.into_iter()
            .filter_map(|dir| access_of(dir).ok().map(|access| (dir, access)))
            .find(|(_, access)| !self.enters_from_user_namespace(*access))
            .map(|(dir, access)| ClosedInstallDir {
                apptainer_dir: resolved_dir.clone(),
                closed_dir: dir.to_path_buf(),
                class_of_root: self.permission_class_on(access),
            })
    }
}

/// Whether `uid_map`, the text of a [`UID_MAP_FILE`], is the map of the
/// initial user namespace.
fn is_initial_user_namespace_map(uid_map: &str) -> bool {
    uid_map.split_whitespace().eq(INITIAL_USER_NAMESPACE_MAP)
}

/// The files that can hold the configuration of the running kernel, in the
/// order they are read.
#[derive(Debug)]
struct KernelConfigFiles {
    /// The gzip file that the kernel itself gives.
    proc_config_gz: PathBuf,
    /// The text file of the distribution, or `None` when the release of the
    /// kernel cannot be read.
    boot_config: Option<PathBuf>,
}

impl KernelConfigFiles {
    /// The files of the host this process runs on.
    fn of_this_host() -> Self {
        let boot_config = fs::read_to_string(KERNEL_RELEASE_FILE)
            .ok()
            .map(|release| Path::new(BOOT_DIR).join(format!("config-{}", release.trim())));
        Self {
            proc_config_gz: PathBuf::from(PROC_KERNEL_CONFIG),
            boot_config,
        }
    }

    /// The first of the files that holds a kernel configuration, or `None`
    /// when neither does.
    fn read(&self) -> Option<KernelConfig> {
        let from_proc = read_gzip_text(&self.proc_config_gz)
            .ok()
            .and_then(KernelConfig::parse);
        if from_proc.is_some() {
            return from_proc;
        }
        let boot_config = self.boot_config.as_deref()?;
        fs::read_to_string(boot_config)
            .ok()
            .and_then(KernelConfig::parse)
    }
}

/// The text of the gzip file at `path`.
fn read_gzip_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    GzDecoder::new(fs::File::open(path)?).read_to_string(&mut text)?;
    Ok(text)
}

/// The configuration a kernel was built with: one `CONFIG_<NAME>=<value>`
/// line for each option that is on.
#[derive(Debug)]
struct KernelConfig(String);

impl KernelConfig {
    /// `None` for a text that sets no option: an empty file, or a file that
    /// is not a kernel configuration.
    fn parse(text: String) -> Option<Self> {
        text.lines()
            .any(|line| line.starts_with(KERNEL_OPTION_PREFIX))
            .then_some(Self(text))
    }

    /// Whether the kernel mounts a squashfs that is packed with zstd.
    fn mounts_zstd_squashfs(&self) -> bool {
        self.0
            .lines()
            .any(|line| line.trim() == ZSTD_SQUASHFS_ENABLED)
    }
}

/// The users that one group of three permission bits of a file is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionClass {
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

/// The owner, the group and the permission bits of a directory.
#[derive(Debug, Clone, Copy)]
struct DirectoryAccess {
    owner: u32,
    group: u32,
    mode: u32,
}

impl DirectoryAccess {
    /// Reads them from the directory at `dir`.
    fn of(dir: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(dir)?;
        Ok(Self {
            owner: metadata.uid(),
            group: metadata.gid(),
            mode: metadata.mode(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptainer::scratch_dir;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::assert_matches;
    use std::io::Write;

    /// Lines of the configuration of a kernel that mounts zstd squashfs.
    const CONFIG_WITH_ZSTD: &str = "\
#
# Automatically generated file; DO NOT EDIT.
#
CONFIG_SQUASHFS=y
CONFIG_SQUASHFS_ZLIB=y
CONFIG_SQUASHFS_ZSTD=y
";

    /// The squashfs lines of the configuration of the Jetson and WSL2
    /// kernels.
    const CONFIG_WITHOUT_ZSTD: &str = "\
CONFIG_SQUASHFS=y
CONFIG_SQUASHFS_ZLIB=y
CONFIG_SQUASHFS_XZ=y
# CONFIG_SQUASHFS_ZSTD is not set
";

    /// The group root.
    const ROOT_GROUP: u32 = 0;

    /// A user of the host that is not root.
    const OTHER_USER: u32 = 1000;

    /// The group of [`OTHER_USER`].
    const OTHER_GROUP: u32 = 1000;

    /// A directory that every user can enter.
    const OPEN_DIR: DirectoryAccess = DirectoryAccess {
        owner: OTHER_USER,
        group: OTHER_GROUP,
        mode: 0o755,
    };

    /// The home directory of a user of Ubuntu 24.04: closed to the other
    /// users.
    const PRIVATE_HOME: DirectoryAccess = DirectoryAccess {
        owner: OTHER_USER,
        group: OTHER_GROUP,
        mode: 0o750,
    };

    /// Root as a system service runs it: in the group root and in no other
    /// group.
    fn root_of_the_root_group() -> RealRoot {
        RealRoot {
            effective_group: ROOT_GROUP,
            supplementary_groups: Vec::new(),
        }
    }

    fn kernel_config(text: &str) -> KernelConfig {
        KernelConfig::parse(text.to_string()).expect("the text sets an option")
    }

    fn write_gzip(path: &Path, text: &str) {
        let mut encoder = GzEncoder::new(
            fs::File::create(path).expect("create the gzip file"),
            Compression::default(),
        );
        encoder
            .write_all(text.as_bytes())
            .expect("write the gzip file");
        encoder.finish().expect("finish the gzip file");
    }

    /// Kernel configuration files in `dir`: the gzip file when `proc_text` is
    /// given, and the text file of the distribution when `boot_text` is.
    fn kernel_config_files(
        dir: &Path,
        proc_text: Option<&str>,
        boot_text: Option<&str>,
    ) -> KernelConfigFiles {
        let proc_config_gz = dir.join("config.gz");
        if let Some(text) = proc_text {
            write_gzip(&proc_config_gz, text);
        }
        let boot_config = dir.join("config-6.8.0-test");
        if let Some(text) = boot_text {
            fs::write(&boot_config, text).expect("write the boot configuration");
        }
        KernelConfigFiles {
            proc_config_gz,
            boot_config: Some(boot_config),
        }
    }

    /// [`UserNamespace::for_daemon`] for an installation in a scratch
    /// directory that every user can enter.
    fn user_namespace_for(
        real_root: Option<&RealRoot>,
        proc_text: Option<&str>,
        boot_text: Option<&str>,
    ) -> UserNamespace {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), proc_text, boot_text);
        UserNamespace::for_daemon(real_root, &files, scratch.path(), |_| Ok(OPEN_DIR))
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
    fn a_configuration_with_the_zstd_line_mounts_zstd_squashfs() {
        assert!(kernel_config(CONFIG_WITH_ZSTD).mounts_zstd_squashfs());
    }

    #[test]
    fn a_configuration_with_the_option_not_set_does_not_mount_zstd_squashfs() {
        assert!(!kernel_config(CONFIG_WITHOUT_ZSTD).mounts_zstd_squashfs());
    }

    #[test]
    fn a_configuration_without_squashfs_does_not_mount_zstd_squashfs() {
        assert!(!kernel_config("CONFIG_EXT4_FS=y\n").mounts_zstd_squashfs());
    }

    #[test]
    fn an_option_with_a_longer_name_is_not_the_zstd_option() {
        let config = kernel_config("CONFIG_SQUASHFS_ZSTD_EXTRA=y\n");
        assert!(!config.mounts_zstd_squashfs());
    }

    #[test]
    fn a_text_that_sets_no_option_is_not_a_kernel_configuration() {
        assert!(KernelConfig::parse(String::new()).is_none());
        assert!(KernelConfig::parse("not a kernel configuration\n".to_string()).is_none());
        assert!(KernelConfig::parse("# CONFIG_SQUASHFS_ZSTD is not set\n".to_string()).is_none());
    }

    #[test]
    fn the_gzip_file_of_the_kernel_is_read_first() {
        let scratch = scratch_dir();
        let files = kernel_config_files(
            scratch.path(),
            Some(CONFIG_WITHOUT_ZSTD),
            Some(CONFIG_WITH_ZSTD),
        );

        let config = files.read().expect("the gzip file holds a configuration");

        assert!(!config.mounts_zstd_squashfs());
    }

    #[test]
    fn the_file_of_the_distribution_is_read_when_the_kernel_gives_none() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, Some(CONFIG_WITHOUT_ZSTD));

        let config = files.read().expect("the boot file holds a configuration");

        assert!(!config.mounts_zstd_squashfs());
    }

    #[test]
    fn the_file_of_the_distribution_is_read_when_the_gzip_file_holds_no_configuration() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(""), Some(CONFIG_WITH_ZSTD));

        let config = files.read().expect("the boot file holds a configuration");

        assert!(config.mounts_zstd_squashfs());
    }

    #[test]
    fn a_file_that_is_not_gzip_is_not_read_as_a_configuration() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, None);
        fs::write(&files.proc_config_gz, CONFIG_WITHOUT_ZSTD).expect("write the plain file");

        assert!(files.read().is_none());
    }

    #[test]
    fn no_configuration_is_read_when_no_file_exists() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, None);

        assert!(files.read().is_none());
    }

    #[test]
    fn no_configuration_is_read_without_the_kernel_release() {
        let scratch = scratch_dir();
        let files = KernelConfigFiles {
            proc_config_gz: scratch.path().join("config.gz"),
            boot_config: None,
        };

        assert!(files.read().is_none());
    }

    #[test]
    fn the_real_root_on_a_kernel_without_zstd_squashfs_forces_the_user_namespace() {
        let root = root_of_the_root_group();

        let from_proc = user_namespace_for(Some(&root), Some(CONFIG_WITHOUT_ZSTD), None);
        let from_boot = user_namespace_for(Some(&root), None, Some(CONFIG_WITHOUT_ZSTD));

        assert_eq!(from_proc, UserNamespace::Forced);
        assert_eq!(from_boot, UserNamespace::Forced);
    }

    #[test]
    fn the_real_root_on_a_kernel_with_zstd_squashfs_keeps_the_default_of_apptainer() {
        let root = root_of_the_root_group();

        let user_namespace = user_namespace_for(Some(&root), Some(CONFIG_WITH_ZSTD), None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn the_real_root_with_no_kernel_configuration_keeps_the_default_of_apptainer() {
        let root = root_of_the_root_group();

        let user_namespace = user_namespace_for(Some(&root), None, None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn a_daemon_that_is_not_the_real_root_keeps_the_default_of_apptainer() {
        let user_namespace = user_namespace_for(None, Some(CONFIG_WITHOUT_ZSTD), None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn a_daemon_that_is_not_the_real_root_reads_no_directory() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let user_namespace =
            UserNamespace::for_daemon(None, &files, Path::new("/no/such/apptainer"), |dir| {
                panic!("the permissions of {} must not be read", dir.display())
            });

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn the_real_root_on_a_kernel_with_zstd_squashfs_reads_no_directory() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITH_ZSTD), None);
        let root = root_of_the_root_group();

        let user_namespace =
            UserNamespace::for_daemon(Some(&root), &files, scratch.path(), |dir| {
                panic!("the permissions of {} must not be read", dir.display())
            });

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn root_enters_a_directory_of_root_and_of_its_effective_group_whatever_its_bits() {
        let closed_to_all = DirectoryAccess {
            owner: ROOT_USER,
            group: ROOT_GROUP,
            mode: 0o000,
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
    fn root_enters_a_directory_of_its_effective_group_by_the_bits_of_the_group() {
        let root = root_of_the_root_group();
        let open_to_group = DirectoryAccess {
            owner: OTHER_USER,
            group: ROOT_GROUP,
            mode: 0o750,
        };
        let open_to_others_only = DirectoryAccess {
            owner: OTHER_USER,
            group: ROOT_GROUP,
            mode: 0o705,
        };

        assert!(root.enters_from_user_namespace(open_to_group));
        assert!(!root.enters_from_user_namespace(open_to_others_only));
    }

    #[test]
    fn root_enters_a_directory_of_a_supplementary_group_by_the_bits_of_the_group() {
        let root = RealRoot {
            effective_group: ROOT_GROUP,
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
        };
        let closed_to_owner = DirectoryAccess {
            owner: ROOT_USER,
            group: OTHER_GROUP,
            mode: 0o077,
        };

        assert!(root.enters_from_user_namespace(open_to_owner));
        assert!(!root.enters_from_user_namespace(closed_to_owner));
    }

    /// The user namespace maps the effective group of the daemon, so the
    /// group root is not in it for a root of another effective group.
    #[test]
    fn root_of_another_effective_group_has_the_rights_of_root_on_that_group_only() {
        let root = RealRoot {
            effective_group: OTHER_GROUP,
            supplementary_groups: Vec::new(),
        };
        let of_root_and_its_effective_group = DirectoryAccess {
            owner: ROOT_USER,
            group: OTHER_GROUP,
            mode: 0o000,
        };
        let of_root_and_the_root_group = DirectoryAccess {
            owner: ROOT_USER,
            group: ROOT_GROUP,
            mode: 0o000,
        };

        assert!(root.enters_from_user_namespace(of_root_and_its_effective_group));
        assert!(!root.enters_from_user_namespace(of_root_and_the_root_group));
    }

    #[test]
    fn an_installation_below_a_closed_directory_is_out_of_reach_of_the_outermost_one() {
        let scratch = scratch_dir();
        let install_dir = scratch.path().join("home/.peppy/bin/apptainer");
        fs::create_dir_all(&install_dir).expect("create the installation directory");
        let resolved_scratch = scratch.path().canonicalize().expect("resolve the scratch");
        let home = resolved_scratch.join("home");
        let peppy_home = home.join(".peppy");
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);
        let root = root_of_the_root_group();

        let user_namespace = UserNamespace::for_daemon(Some(&root), &files, &install_dir, |dir| {
            let is_closed = dir == home || dir == peppy_home;
            Ok(if is_closed { PRIVATE_HOME } else { OPEN_DIR })
        });

        assert_eq!(
            user_namespace,
            UserNamespace::OutOfReach(ClosedInstallDir {
                apptainer_dir: resolved_scratch.join("home/.peppy/bin/apptainer"),
                closed_dir: home,
                class_of_root: PermissionClass::Others,
            })
        );
    }

    #[test]
    fn the_same_installation_is_in_reach_of_a_root_in_the_group_of_the_closed_directory() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);
        let root = RealRoot {
            effective_group: ROOT_GROUP,
            supplementary_groups: vec![OTHER_GROUP],
        };

        let user_namespace =
            UserNamespace::for_daemon(Some(&root), &files, scratch.path(), |_| Ok(PRIVATE_HOME));

        assert_eq!(user_namespace, UserNamespace::Forced);
    }

    #[test]
    fn a_closed_directory_of_a_group_of_root_is_opened_for_the_group() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);
        let root = root_of_the_root_group();

        let user_namespace = UserNamespace::for_daemon(Some(&root), &files, scratch.path(), |_| {
            Ok(DirectoryAccess {
                owner: OTHER_USER,
                group: ROOT_GROUP,
                mode: 0o700,
            })
        });

        assert_matches!(
            user_namespace,
            UserNamespace::OutOfReach(ClosedInstallDir {
                class_of_root: PermissionClass::Group,
                ..
            })
        );
    }

    #[test]
    fn a_directory_whose_permissions_cannot_be_read_decides_nothing() {
        let scratch = scratch_dir();
        let resolved_scratch = scratch.path().canonicalize().expect("resolve the scratch");
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);
        let root = root_of_the_root_group();
        let unreadable = || Err(io::Error::from(io::ErrorKind::PermissionDenied));

        let all_unreadable =
            UserNamespace::for_daemon(Some(&root), &files, scratch.path(), |_| unreadable());
        let closed_below_an_unreadable_one =
            UserNamespace::for_daemon(Some(&root), &files, scratch.path(), |dir| {
                if dir == resolved_scratch {
                    Ok(PRIVATE_HOME)
                } else {
                    unreadable()
                }
            });

        assert_eq!(all_unreadable, UserNamespace::Forced);
        assert_matches!(
            closed_below_an_unreadable_one,
            UserNamespace::OutOfReach(ClosedInstallDir { closed_dir, .. })
                if closed_dir == resolved_scratch
        );
    }

    #[test]
    fn an_installation_that_cannot_be_resolved_decides_nothing() {
        let scratch = scratch_dir();
        let missing = scratch.path().join("no-such-installation");
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);
        let root = root_of_the_root_group();

        let user_namespace =
            UserNamespace::for_daemon(Some(&root), &files, &missing, |_| Ok(PRIVATE_HOME));

        assert_eq!(user_namespace, UserNamespace::Forced);
    }

    #[test]
    fn the_default_of_apptainer_passes_no_flag() {
        assert_matches!(UserNamespace::ApptainerDefault.flag(), Ok(None));
    }

    #[test]
    fn a_forced_user_namespace_passes_the_flag() {
        assert_matches!(UserNamespace::Forced.flag(), Ok(Some("--userns")));
    }

    #[test]
    fn an_installation_out_of_reach_gives_the_command_that_opens_the_directory() {
        let out_of_reach = UserNamespace::OutOfReach(ClosedInstallDir {
            apptainer_dir: PathBuf::from("/home/user/.peppy/bin/apptainer"),
            closed_dir: PathBuf::from("/home/user"),
            class_of_root: PermissionClass::Others,
        });

        let error = out_of_reach
            .flag()
            .expect_err("no container can start from an installation out of reach");

        assert_matches!(
            &error,
            Error::InstallDirClosedToRootInUserNamespace {
                apptainer_dir,
                closed_dir,
                chmod_class: 'o',
            } if closed_dir == "/home/user" && apptainer_dir == "/home/user/.peppy/bin/apptainer"
        );
        let message = error.to_string();
        assert!(
            message.contains("chmod o+x /home/user\n"),
            "the message gives the command that opens the directory: {message}"
        );
    }

    #[test]
    fn the_permissions_of_a_real_directory_are_read() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = scratch_dir();
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o750))
            .expect("set the mode of the scratch directory");

        let access = DirectoryAccess::of(scratch.path()).expect("read the scratch directory");

        assert_eq!(access.mode & 0o777, 0o750);
        assert_eq!(access.owner, rustix::process::geteuid().as_raw());
    }
}
