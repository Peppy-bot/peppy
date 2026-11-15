//! The squashfs compressions that the running kernel mounts.
//!
//! The configuration that a kernel was built with has one
//! `CONFIG_<NAME>=<value>` line for each option that is on. The squashfs
//! driver has one option for each compression that it reads, and some
//! kernels are built without one: the WSL2 kernel of Microsoft and the kernel
//! of the NVIDIA Jetson have `# CONFIG_SQUASHFS_ZSTD is not set`.

use super::squashfs_image::SquashfsCompression;
use flate2::read::GzDecoder;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// The line of a kernel configuration that puts `compression` into the
/// squashfs driver, or `None` for a compression that no kernel reads. Each
/// option is a boolean, so no other value turns it on.
fn enabling_line(compression: SquashfsCompression) -> Option<&'static str> {
    match compression {
        SquashfsCompression::Gzip => Some("CONFIG_SQUASHFS_ZLIB=y"),
        SquashfsCompression::Lzma => None,
        SquashfsCompression::Lzo => Some("CONFIG_SQUASHFS_LZO=y"),
        SquashfsCompression::Xz => Some("CONFIG_SQUASHFS_XZ=y"),
        SquashfsCompression::Lz4 => Some("CONFIG_SQUASHFS_LZ4=y"),
        SquashfsCompression::Zstd => Some("CONFIG_SQUASHFS_ZSTD=y"),
    }
}

/// The squashfs compressions that a kernel mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KernelSquashfs {
    mounted_compressions: Vec<SquashfsCompression>,
}

impl KernelSquashfs {
    /// Those of the kernel that this process runs on, or `None` when its
    /// configuration cannot be read.
    ///
    /// The configuration of a running kernel does not change, so the process
    /// reads it once.
    pub(crate) fn of_this_host() -> Option<&'static Self> {
        static OF_THIS_HOST: OnceLock<Option<KernelSquashfs>> = OnceLock::new();
        OF_THIS_HOST
            .get_or_init(|| KernelConfigFiles::of_this_host().read())
            .as_ref()
    }

    /// Reads them from `config`, the text of a kernel configuration. `None`
    /// for a text that sets no option: an empty file, or a file that is not
    /// a kernel configuration.
    fn parse(config: &str) -> Option<Self> {
        let sets_an_option = config
            .lines()
            .any(|line| line.starts_with(KERNEL_OPTION_PREFIX));
        if !sets_an_option {
            return None;
        }
        let is_enabled = |compression: &SquashfsCompression| {
            enabling_line(*compression).is_some_and(|enabling_line| {
                config.lines().any(|line| line.trim() == enabling_line)
            })
        };
        Some(Self {
            mounted_compressions: SquashfsCompression::ALL
                .into_iter()
                .filter(is_enabled)
                .collect(),
        })
    }

    /// Whether the kernel mounts a squashfs that is packed with
    /// `compression`.
    pub(crate) fn mounts(&self, compression: SquashfsCompression) -> bool {
        self.mounted_compressions.contains(&compression)
    }
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

    /// What the first of the files that holds a kernel configuration tells,
    /// or `None` when neither holds one.
    fn read(&self) -> Option<KernelSquashfs> {
        let from_proc = read_gzip_text(&self.proc_config_gz)
            .ok()
            .and_then(|config| KernelSquashfs::parse(&config));
        if from_proc.is_some() {
            return from_proc;
        }
        let boot_config = self.boot_config.as_deref()?;
        let config = fs::read_to_string(boot_config).ok()?;
        KernelSquashfs::parse(&config)
    }
}

/// The text of the gzip file at `path`.
fn read_gzip_text(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    GzDecoder::new(fs::File::open(path)?).read_to_string(&mut text)?;
    Ok(text)
}

/// Kernel configurations for tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::KernelSquashfs;

    /// Lines of the configuration of a kernel that mounts zstd squashfs.
    pub(crate) const CONFIG_WITH_ZSTD: &str = "\
#
# Automatically generated file; DO NOT EDIT.
#
CONFIG_SQUASHFS=y
CONFIG_SQUASHFS_ZLIB=y
CONFIG_SQUASHFS_ZSTD=y
";

    /// The squashfs lines of the configuration of the Jetson and WSL2
    /// kernels.
    pub(crate) const CONFIG_WITHOUT_ZSTD: &str = "\
CONFIG_SQUASHFS=y
CONFIG_SQUASHFS_ZLIB=y
CONFIG_SQUASHFS_XZ=y
# CONFIG_SQUASHFS_ZSTD is not set
";

    /// What `config`, the text of a kernel configuration, tells.
    pub(crate) fn kernel_with_config(config: &str) -> KernelSquashfs {
        KernelSquashfs::parse(config).expect("the text sets an option")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{CONFIG_WITH_ZSTD, CONFIG_WITHOUT_ZSTD, kernel_with_config};
    use super::*;
    use crate::apptainer::scratch_dir;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

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

    #[test]
    fn a_kernel_mounts_the_compressions_that_its_configuration_turns_on() {
        let kernel = kernel_with_config(CONFIG_WITHOUT_ZSTD);

        assert!(kernel.mounts(SquashfsCompression::Gzip));
        assert!(kernel.mounts(SquashfsCompression::Xz));
        assert!(!kernel.mounts(SquashfsCompression::Zstd));
        assert!(!kernel.mounts(SquashfsCompression::Lzo));
        assert!(!kernel.mounts(SquashfsCompression::Lz4));
    }

    #[test]
    fn a_configuration_with_the_zstd_line_mounts_zstd_squashfs() {
        assert!(kernel_with_config(CONFIG_WITH_ZSTD).mounts(SquashfsCompression::Zstd));
    }

    #[test]
    fn a_configuration_without_squashfs_mounts_no_compression() {
        let kernel = kernel_with_config("CONFIG_EXT4_FS=y\n");

        for compression in SquashfsCompression::ALL {
            assert!(
                !kernel.mounts(compression),
                "a kernel without squashfs mounts no {}",
                compression.name()
            );
        }
    }

    #[test]
    fn an_option_with_a_longer_name_is_not_the_zstd_option() {
        let kernel = kernel_with_config("CONFIG_SQUASHFS_ZSTD_EXTRA=y\n");

        assert!(!kernel.mounts(SquashfsCompression::Zstd));
    }

    /// No kernel has an option that reads the first lzma format.
    #[test]
    fn no_kernel_mounts_lzma() {
        let every_option = "\
CONFIG_SQUASHFS_ZLIB=y
CONFIG_SQUASHFS_LZ4=y
CONFIG_SQUASHFS_LZO=y
CONFIG_SQUASHFS_XZ=y
CONFIG_SQUASHFS_ZSTD=y
";

        let kernel = kernel_with_config(every_option);

        assert!(!kernel.mounts(SquashfsCompression::Lzma));
        assert!(kernel.mounts(SquashfsCompression::Lz4));
        assert!(kernel.mounts(SquashfsCompression::Lzo));
    }

    #[test]
    fn a_text_that_sets_no_option_is_not_a_kernel_configuration() {
        assert_eq!(KernelSquashfs::parse(""), None);
        assert_eq!(KernelSquashfs::parse("not a kernel configuration\n"), None);
        assert_eq!(
            KernelSquashfs::parse("# CONFIG_SQUASHFS_ZSTD is not set\n"),
            None
        );
    }

    #[test]
    fn the_gzip_file_of_the_kernel_is_read_first() {
        let scratch = scratch_dir();
        let files = kernel_config_files(
            scratch.path(),
            Some(CONFIG_WITHOUT_ZSTD),
            Some(CONFIG_WITH_ZSTD),
        );

        let kernel = files.read().expect("the gzip file holds a configuration");

        assert!(!kernel.mounts(SquashfsCompression::Zstd));
    }

    #[test]
    fn the_file_of_the_distribution_is_read_when_the_kernel_gives_none() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, Some(CONFIG_WITHOUT_ZSTD));

        let kernel = files.read().expect("the boot file holds a configuration");

        assert!(!kernel.mounts(SquashfsCompression::Zstd));
    }

    #[test]
    fn the_file_of_the_distribution_is_read_when_the_gzip_file_holds_no_configuration() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), Some(""), Some(CONFIG_WITH_ZSTD));

        let kernel = files.read().expect("the boot file holds a configuration");

        assert!(kernel.mounts(SquashfsCompression::Zstd));
    }

    #[test]
    fn a_file_that_is_not_gzip_is_not_read_as_a_configuration() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, None);
        fs::write(&files.proc_config_gz, CONFIG_WITHOUT_ZSTD).expect("write the plain file");

        assert_eq!(files.read(), None);
    }

    #[test]
    fn no_configuration_is_read_when_no_file_exists() {
        let scratch = scratch_dir();
        let files = kernel_config_files(scratch.path(), None, None);

        assert_eq!(files.read(), None);
    }

    #[test]
    fn no_configuration_is_read_without_the_kernel_release() {
        let scratch = scratch_dir();
        let files = KernelConfigFiles {
            proc_config_gz: scratch.path().join("config.gz"),
            boot_config: None,
        };

        assert_eq!(files.read(), None);
    }
}
