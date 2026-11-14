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
//! The namespace maps user 0 and group 0 only. In it:
//!
//! - A container has no capability of the real root. It cannot, for example,
//!   take a real-time priority.
//! - Root has its rights on the files of root only, and is one of the other
//!   users for every other file. An apptainer installation below a directory
//!   that is closed to the other users (the home directory of another user,
//!   of mode 750) is thus out of its reach, and
//!   [`UserNamespace::for_this_daemon`] reports that directory.

use crate::error::{Error, Result};
use flate2::read::GzDecoder;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Once;

/// The flag of `apptainer run` and `apptainer exec` that puts the container
/// in a user namespace.
pub(crate) const USER_NAMESPACE_FLAG: &str = "--userns";

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

/// The user and the group of root.
const ROOT_ID: u32 = 0;

/// Whether peppy asks apptainer for a user namespace for a `run` or an
/// `exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserNamespace {
    /// peppy passes no flag: apptainer uses a user namespace for a user that
    /// is not root, and none for root.
    ApptainerDefault,
    /// peppy passes [`USER_NAMESPACE_FLAG`]: the daemon is the real root and
    /// its kernel cannot mount the zstd squashfs of an image.
    Forced,
}

impl UserNamespace {
    /// The choice for the daemon of this process on this host.
    ///
    /// # Errors
    ///
    /// When the choice is [`UserNamespace::Forced`] and root cannot reach
    /// `apptainer_dir` from the user namespace, or the permissions of a
    /// directory on the way to it cannot be read.
    pub(crate) fn for_this_daemon(apptainer_dir: &Path) -> Result<Self> {
        let user_namespace = Self::for_daemon(
            daemon_is_real_root(),
            &KernelConfigFiles::of_this_host(),
            apptainer_dir,
            DirectoryAccess::of,
        )?;
        if user_namespace == Self::Forced {
            announce_forced_user_namespace();
        }
        Ok(user_namespace)
    }

    /// [`UserNamespace::for_this_daemon`] for a daemon and a host that the
    /// arguments describe. `access_of` reads the owner, the group and the
    /// permission bits of a directory.
    fn for_daemon(
        daemon_is_real_root: bool,
        kernel_config_files: &KernelConfigFiles,
        apptainer_dir: &Path,
        access_of: impl Fn(&Path) -> io::Result<DirectoryAccess>,
    ) -> Result<Self> {
        if !daemon_is_real_root {
            return Ok(Self::ApptainerDefault);
        }
        // Without a configuration to read, the kernel is taken to mount zstd
        // squashfs, as the kernel of each Ubuntu and Debian release does.
        let Some(kernel_config) = kernel_config_files.read() else {
            return Ok(Self::ApptainerDefault);
        };
        if kernel_config.mounts_zstd_squashfs() {
            return Ok(Self::ApptainerDefault);
        }
        ensure_root_reaches_from_user_namespace(apptainer_dir, access_of)?;
        Ok(Self::Forced)
    }
}

/// Whether this process is the real root: user 0 of the initial user
/// namespace. A process whose user namespace cannot be read is taken not to
/// be, so that peppy passes no flag for it.
fn daemon_is_real_root() -> bool {
    rustix::process::geteuid().is_root()
        && fs::read_to_string(UID_MAP_FILE)
            .is_ok_and(|uid_map| is_initial_user_namespace_map(&uid_map))
}

/// Whether `uid_map`, the text of a [`UID_MAP_FILE`], is the map of the
/// initial user namespace.
fn is_initial_user_namespace_map(uid_map: &str) -> bool {
    uid_map.split_whitespace().eq(INITIAL_USER_NAMESPACE_MAP)
}

/// Tells the operator, once for the process, why the containers of this root
/// daemon run without the capabilities of the real root.
fn announce_forced_user_namespace() {
    static ANNOUNCED: Once = Once::new();
    ANNOUNCED.call_once(|| {
        tracing::info!(
            "This daemon runs as root and the kernel cannot mount zstd squashfs, so \
             apptainer runs each container in a user namespace ({USER_NAMESPACE_FLAG}): a \
             node has no capability of the real root"
        );
    });
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

    /// The class of permission bits that the kernel reads for root: the bits
    /// of the owner for a directory of root, the bits of the group for a
    /// directory of the group of root, and the bits of the other users for
    /// every other directory.
    fn class_of_root(self) -> PermissionClass {
        if self.owner == ROOT_ID {
            PermissionClass::Owner
        } else if self.group == ROOT_ID {
            PermissionClass::Group
        } else {
            PermissionClass::Others
        }
    }

    /// Whether root can enter the directory from a user namespace that maps
    /// user 0 and group 0 only.
    ///
    /// Outside a user namespace, the capability `CAP_DAC_READ_SEARCH` lets
    /// root enter a directory whatever its permission bits. In the namespace
    /// the capability holds only for a directory whose owner and group are
    /// both mapped, so the bits of [`class_of_root`](Self::class_of_root)
    /// decide for every other directory.
    fn admits_root_in_user_namespace(self) -> bool {
        let owner_and_group_are_mapped = self.owner == ROOT_ID && self.group == ROOT_ID;
        let bits_admit_root = self.mode & self.class_of_root().search_bit() != 0;
        owner_and_group_are_mapped || bits_admit_root
    }
}

/// Checks that root can reach `apptainer_dir` from the user namespace: that
/// it can enter the directory and each directory above it.
fn ensure_root_reaches_from_user_namespace(
    apptainer_dir: &Path,
    access_of: impl Fn(&Path) -> io::Result<DirectoryAccess>,
) -> Result<()> {
    let unreadable = |path: &Path, source: io::Error| Error::InstallDirAccessUnreadable {
        path: path.display().to_string(),
        source,
    };
    // The kernel resolves symbolic links, so the check reads the directories
    // of the resolved path.
    let resolved_dir = apptainer_dir
        .canonicalize()
        .map_err(|source| unreadable(apptainer_dir, source))?;
    // From the root of the file system down, so that the error names the
    // outermost closed directory: the first one to open.
    let mut dirs: Vec<&Path> = resolved_dir.ancestors().collect();
    dirs.reverse();
    for dir in dirs {
        let access = access_of(dir).map_err(|source| unreadable(dir, source))?;
        if !access.admits_root_in_user_namespace() {
            return Err(Error::InstallDirClosedToRootInUserNamespace {
                apptainer_dir: resolved_dir.display().to_string(),
                closed_dir: dir.display().to_string(),
                chmod_class: access.class_of_root().chmod_letter(),
            });
        }
    }
    Ok(())
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

    /// A directory that every user can enter.
    const OPEN_DIR: DirectoryAccess = DirectoryAccess {
        owner: 1000,
        group: 1000,
        mode: 0o755,
    };

    /// The home directory of a user of Ubuntu 24.04: closed to the other
    /// users.
    const PRIVATE_HOME: DirectoryAccess = DirectoryAccess {
        owner: 1000,
        group: 1000,
        mode: 0o750,
    };

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
        daemon_is_real_root: bool,
        proc_text: Option<&str>,
        boot_text: Option<&str>,
    ) -> UserNamespace {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), proc_text, boot_text);
        UserNamespace::for_daemon(daemon_is_real_root, &files, scratch.path(), |_| {
            Ok(OPEN_DIR)
        })
        .expect("root reaches a directory that every user can enter")
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
        let from_proc = user_namespace_for(true, Some(CONFIG_WITHOUT_ZSTD), None);
        let from_boot = user_namespace_for(true, None, Some(CONFIG_WITHOUT_ZSTD));

        assert_eq!(from_proc, UserNamespace::Forced);
        assert_eq!(from_boot, UserNamespace::Forced);
    }

    #[test]
    fn the_real_root_on_a_kernel_with_zstd_squashfs_keeps_the_default_of_apptainer() {
        let user_namespace = user_namespace_for(true, Some(CONFIG_WITH_ZSTD), None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn the_real_root_with_no_kernel_configuration_keeps_the_default_of_apptainer() {
        let user_namespace = user_namespace_for(true, None, None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn a_daemon_that_is_not_the_real_root_keeps_the_default_of_apptainer() {
        let user_namespace = user_namespace_for(false, Some(CONFIG_WITHOUT_ZSTD), None);

        assert_eq!(user_namespace, UserNamespace::ApptainerDefault);
    }

    #[test]
    fn a_daemon_that_is_not_the_real_root_reads_no_directory() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let user_namespace =
            UserNamespace::for_daemon(false, &files, Path::new("/no/such/apptainer"), |dir| {
                panic!("the permissions of {} must not be read", dir.display())
            });

        assert_matches!(user_namespace, Ok(UserNamespace::ApptainerDefault));
    }

    #[test]
    fn root_enters_a_directory_of_root_whatever_its_bits() {
        let closed_to_all = DirectoryAccess {
            owner: ROOT_ID,
            group: ROOT_ID,
            mode: 0o000,
        };

        assert!(closed_to_all.admits_root_in_user_namespace());
    }

    #[test]
    fn root_enters_a_directory_of_another_user_by_the_bits_of_the_other_users() {
        assert!(OPEN_DIR.admits_root_in_user_namespace());
        assert!(!PRIVATE_HOME.admits_root_in_user_namespace());
    }

    #[test]
    fn root_enters_a_directory_of_the_group_of_root_by_the_bits_of_the_group() {
        let open_to_group = DirectoryAccess {
            owner: 1000,
            group: ROOT_ID,
            mode: 0o750,
        };
        let open_to_others_only = DirectoryAccess {
            owner: 1000,
            group: ROOT_ID,
            mode: 0o705,
        };

        assert!(open_to_group.admits_root_in_user_namespace());
        assert!(!open_to_others_only.admits_root_in_user_namespace());
    }

    #[test]
    fn root_enters_its_own_directory_of_another_group_by_the_bits_of_the_owner() {
        let open_to_owner = DirectoryAccess {
            owner: ROOT_ID,
            group: 1000,
            mode: 0o700,
        };
        let closed_to_owner = DirectoryAccess {
            owner: ROOT_ID,
            group: 1000,
            mode: 0o077,
        };

        assert!(open_to_owner.admits_root_in_user_namespace());
        assert!(!closed_to_owner.admits_root_in_user_namespace());
    }

    #[test]
    fn an_installation_below_a_closed_directory_names_the_outermost_one() {
        let scratch = scratch_dir();
        let install_dir = scratch.path().join("home/.peppy/bin/apptainer");
        fs::create_dir_all(&install_dir).expect("create the installation directory");
        let resolved_scratch = scratch.path().canonicalize().expect("resolve the scratch");
        let home = resolved_scratch.join("home");
        let peppy_home = home.join(".peppy");
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let error = UserNamespace::for_daemon(true, &files, &install_dir, |dir| {
            let is_closed = dir == home || dir == peppy_home;
            Ok(if is_closed { PRIVATE_HOME } else { OPEN_DIR })
        })
        .expect_err("root cannot enter the home directory");

        assert_matches!(
            &error,
            Error::InstallDirClosedToRootInUserNamespace {
                apptainer_dir,
                closed_dir,
                chmod_class: 'o',
            } if *closed_dir == home.display().to_string()
                && *apptainer_dir == resolved_scratch.join("home/.peppy/bin/apptainer").display().to_string()
        );
        let message = error.to_string();
        assert!(
            message.contains(&format!("chmod o+x {}", home.display())),
            "the message gives the command that opens the directory: {message}"
        );
    }

    #[test]
    fn a_closed_directory_of_the_group_of_root_is_opened_for_the_group() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let error = UserNamespace::for_daemon(true, &files, scratch.path(), |_| {
            Ok(DirectoryAccess {
                owner: 1000,
                group: ROOT_ID,
                mode: 0o700,
            })
        })
        .expect_err("root cannot enter a directory that is closed to its group");

        assert_matches!(
            error,
            Error::InstallDirClosedToRootInUserNamespace {
                chmod_class: 'g',
                ..
            }
        );
    }

    #[test]
    fn a_directory_whose_permissions_cannot_be_read_is_named() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let error = UserNamespace::for_daemon(true, &files, scratch.path(), |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .expect_err("the permissions cannot be read");

        assert_matches!(
            error,
            Error::InstallDirAccessUnreadable { path, .. } if path == "/"
        );
    }

    #[test]
    fn an_installation_that_does_not_exist_is_named() {
        let scratch = scratch_dir();
        let missing = scratch.path().join("no-such-installation");
        let files = kernel_config_files(scratch.path(), Some(CONFIG_WITHOUT_ZSTD), None);

        let error = UserNamespace::for_daemon(true, &files, &missing, |_| Ok(OPEN_DIR))
            .expect_err("the installation cannot be resolved");

        assert_matches!(
            error,
            Error::InstallDirAccessUnreadable { path, .. }
                if path == missing.display().to_string()
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
