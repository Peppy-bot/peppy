//! The NVIDIA L4T driver libraries that `--nv` binds on a Jetson.
//!
//! Apptainer's `--nv` binds the host libraries that its
//! `etc/apptainer/nvliblist.conf` names. It resolves each name through the
//! host's `ldconfig -p`, and binds every library whose name starts with it
//! into the container's `/.singularity.d/libs`. The stock list names the
//! libraries of NVIDIA's desktop driver. The L4T driver of a Jetson (NVIDIA
//! Tegra) loads more, and the set changes from one L4T release to the next.
//! For example, its `libcuda.so.1` opens `libnvcucompat.so` and
//! `libnvcuextend.so` when it creates a context, and its EGL, GLX and NVML
//! libraries link against `libnvidia-rmapi-tegra.so`. In a container without
//! them, the first CUDA allocation crashes the process with SIGSEGV, and EGL
//! falls through to Mesa.
//!
//! Each L4T release lists the files a container needs in its container
//! manifest: the `*.csv` files in [`L4T_MANIFEST_DIR`], which NVIDIA's own
//! container runtime reads. So before each command that passes `--nv`, peppy
//! writes the file name of every shared library in that manifest into a block
//! of the list that peppy owns. Apptainer then binds them as it binds its
//! stock entries, and binds a name only once when several entries match it.
//! On a host without the manifest (every host that is not a Jetson), the list
//! stays as it is.

use super::atomic_file::replace_file;
use crate::error::{Error, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Directory of the L4T container manifest on a Jetson.
pub(crate) const L4T_MANIFEST_DIR: &str =
    "/etc/nvidia-container-runtime/host-files-for-container.d";

/// Path of the `--nv` library list inside an apptainer install tree.
pub(crate) const NVLIBLIST_PATH: &str = "etc/apptainer/nvliblist.conf";

/// First line of the block of the list that peppy owns.
const BLOCK_BEGIN: &str =
    "# BEGIN peppy: NVIDIA L4T driver libraries from the host's container manifest";

/// Last line of the block of the list that peppy owns.
const BLOCK_END: &str = "# END peppy: NVIDIA L4T driver libraries";

/// What one manifest entry puts into a container, by its keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// `dev`: a device node.
    Device,
    /// `lib`: a file (a shared library, a program or a data file), mounted at
    /// its host path.
    File,
    /// `sym`: a symbolic link, made again at its host path.
    Symlink,
    /// `dir`: a directory, mounted at its host path.
    Directory,
}

/// One `<kind>, <path>` line of the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    kind: EntryKind,
    path: PathBuf,
}

impl ManifestEntry {
    /// Parses one line that is neither blank nor a comment.
    fn parse(line: &str) -> std::result::Result<Self, String> {
        let (keyword, path) = line
            .split_once(',')
            .ok_or_else(|| format!("`{line}` is not `<kind>, <path>`"))?;
        let kind = match keyword.trim() {
            "dev" => EntryKind::Device,
            "lib" => EntryKind::File,
            "sym" => EntryKind::Symlink,
            "dir" => EntryKind::Directory,
            other => {
                return Err(format!(
                    "`{other}` is not an entry kind; the kinds are dev, lib, sym and dir"
                ));
            }
        };
        let path = path.trim();
        if path.is_empty() {
            return Err(format!("`{line}` has no path"));
        }
        Ok(Self {
            kind,
            path: PathBuf::from(path),
        })
    }

    /// The file name that the container's loader finds this entry by, when
    /// the entry is a shared library. A `sym` entry gives the name of its
    /// link (`libnvidia-ptxjitcompiler.so.1`), which is a name that a library
    /// asks the loader for.
    fn shared_library_name(&self) -> Option<&str> {
        if !matches!(self.kind, EntryKind::File | EntryKind::Symlink) {
            return None;
        }
        let name = self.path.file_name()?.to_str()?;
        is_shared_library_file_name(name).then_some(name)
    }
}

/// Whether `name` is the file name of a shared library: `<stem>.so`, or
/// `<stem>.so.<version>` where the version is dot-separated numbers.
fn is_shared_library_file_name(name: &str) -> bool {
    let Some((stem, version)) = name.rsplit_once(".so") else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    if version.is_empty() {
        return true;
    }
    let Some(numbers) = version.strip_prefix('.') else {
        return false;
    };
    numbers
        .split('.')
        .all(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// Parses the text of one manifest file. Blank lines and `#` comments are
/// skipped; any other line that does not parse fails the whole file.
fn parse_manifest(text: &str, file: &Path) -> Result<Vec<ManifestEntry>> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(number, line)| {
            ManifestEntry::parse(line).map_err(|reason| Error::L4tManifestInvalid {
                path: file.display().to_string(),
                line: number,
                reason,
            })
        })
        .collect()
}

/// Reads every `*.csv` file of `manifest_dir`, in file-name order. `None` when
/// there is no directory at that path: the host is not a Jetson.
fn read_manifest(manifest_dir: &Path) -> Result<Option<Vec<ManifestEntry>>> {
    let unreadable = |path: &Path| {
        let path = path.display().to_string();
        move |source| Error::L4tManifestUnreadable { path, source }
    };
    let dir_entries = match fs::read_dir(manifest_dir) {
        Ok(dir_entries) => dir_entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(source) => return Err(unreadable(manifest_dir)(source)),
    };
    let mut files = Vec::new();
    for dir_entry in dir_entries {
        let path = dir_entry.map_err(unreadable(manifest_dir))?.path();
        if path.extension().is_some_and(|extension| extension == "csv") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();

    let mut entries = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).map_err(unreadable(&file))?;
        entries.extend(parse_manifest(&text, &file)?);
    }
    Ok(Some(entries))
}

/// The file names of the manifest's shared libraries, each name once, in the
/// order of its first entry.
fn shared_library_names(entries: &[ManifestEntry]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in entries
        .iter()
        .filter_map(ManifestEntry::shared_library_name)
    {
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// `nvliblist` with its peppy block holding `names`, or with no peppy block
/// when `names` is empty. The block goes at the end, after one blank line.
/// The lines outside the block stay as they are, except that trailing blank
/// lines become one line end. A block that has no end line runs to the end of
/// the file. A list without a block, given no names, is returned exactly as it
/// is, so a host whose manifest holds no shared library never gets its list
/// written.
fn with_l4t_block(nvliblist: &str, names: &[String]) -> String {
    let mut outside_block = Vec::new();
    let mut has_block = false;
    let mut in_block = false;
    for line in nvliblist.lines() {
        match line.trim_end() {
            BLOCK_BEGIN => {
                has_block = true;
                in_block = true;
            }
            BLOCK_END if in_block => in_block = false,
            _ if in_block => {}
            _ => outside_block.push(line),
        }
    }
    if names.is_empty() && !has_block {
        return nvliblist.to_string();
    }
    let mut out = outside_block.join("\n").trim_end().to_string();
    out.push('\n');
    if names.is_empty() {
        return out;
    }
    out.push('\n');
    out.push_str(BLOCK_BEGIN);
    out.push('\n');
    for name in names {
        out.push_str(name);
        out.push('\n');
    }
    out.push_str(BLOCK_END);
    out.push('\n');
    out
}

/// What [`sync_nvliblist`] did to the list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NvliblistSync {
    /// The host has no L4T manifest; the list is untouched.
    NoManifest,
    /// The list already holds the manifest's shared libraries.
    Unchanged,
    /// The list was replaced, and now holds this many manifest libraries.
    Updated { libraries: usize },
}

/// Makes the peppy block of the list at `nvliblist` hold the shared libraries
/// of the L4T manifest in `manifest_dir`. The list is replaced whole (see
/// [`replace_file`]) and keeps its mode, so an apptainer that starts at the
/// same time reads the old list or the new one. The list is written only when
/// its contents change.
pub(crate) fn sync_nvliblist(manifest_dir: &Path, nvliblist: &Path) -> Result<NvliblistSync> {
    let Some(entries) = read_manifest(manifest_dir)? else {
        return Ok(NvliblistSync::NoManifest);
    };
    let names = shared_library_names(&entries);
    let update_failed = |source| Error::NvliblistUpdateFailed {
        path: nvliblist.display().to_string(),
        source,
    };
    let current = fs::read_to_string(nvliblist).map_err(update_failed)?;
    let wanted = with_l4t_block(&current, &names);
    if wanted == current {
        return Ok(NvliblistSync::Unchanged);
    }
    let mode = fs::metadata(nvliblist)
        .map_err(update_failed)?
        .permissions()
        .mode();
    replace_file(nvliblist, wanted.as_bytes(), mode & 0o7777).map_err(update_failed)?;
    Ok(NvliblistSync::Updated {
        libraries: names.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    fn scratch() -> TempDir {
        TempDir::new_in(config_test_support::test_tmp_root()).expect("create scratch dir")
    }

    /// Part of apptainer's stock list: a comment, a program and libraries.
    const STOCK_LIST: &str = "\
# put binaries here
nvidia-smi

# put libs here (must end in .so)
libcuda.so
libEGL_nvidia.so
";

    fn entry(kind: EntryKind, path: &str) -> ManifestEntry {
        ManifestEntry {
            kind,
            path: PathBuf::from(path),
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    // -- parsing --------------------------------------------------------------

    #[test]
    fn every_entry_kind_parses_and_whitespace_around_the_fields_is_dropped() {
        let text = "\
dev, /dev/dri/card*
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvos.so
sym,/usr/lib/aarch64-linux-gnu/libnvcucompat.so
  dir ,  /dev/dri/by-path
";
        let entries = parse_manifest(text, Path::new("drivers.csv")).unwrap();
        assert_eq!(
            entries,
            [
                entry(EntryKind::Device, "/dev/dri/card*"),
                entry(
                    EntryKind::File,
                    "/usr/lib/aarch64-linux-gnu/nvidia/libnvos.so"
                ),
                entry(
                    EntryKind::Symlink,
                    "/usr/lib/aarch64-linux-gnu/libnvcucompat.so"
                ),
                entry(EntryKind::Directory, "/dev/dri/by-path"),
            ]
        );
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        let text = "\n# NVIDIA drivers\n   \nlib, /usr/lib/libnvos.so\n  # indented comment\n";
        let entries = parse_manifest(text, Path::new("drivers.csv")).unwrap();
        assert_eq!(entries, [entry(EntryKind::File, "/usr/lib/libnvos.so")]);
    }

    #[test]
    fn a_bad_line_fails_the_file_naming_the_file_and_the_line() {
        for (text, line, reason_part) in [
            ("lib, /a.so\nlib /b.so\n", 2, "is not `<kind>, <path>`"),
            ("\n\nfile, /a.so\n", 3, "`file` is not an entry kind"),
            ("lib, \n", 1, "has no path"),
        ] {
            let error = parse_manifest(text, Path::new("/etc/l4t.csv")).unwrap_err();
            let Error::L4tManifestInvalid {
                path,
                line: at,
                reason,
            } = &error
            else {
                panic!("expected L4tManifestInvalid, got {error:?}");
            };
            assert_eq!((path.as_str(), *at), ("/etc/l4t.csv", line), "for {text:?}");
            assert!(reason.contains(reason_part), "{reason:?} for {text:?}");
        }
    }

    // -- shared library names -------------------------------------------------

    #[test]
    fn shared_library_file_names_are_told_from_other_files() {
        for name in [
            "libnvos.so",
            "libcuda.so.1",
            "libnvidia-nvvm.so.595.78",
            "libv4l2.so.0.0.999999",
            "nvidia-drm_gbm.so",
            "libfoo.solver.so",
        ] {
            assert!(is_shared_library_file_name(name), "{name}");
        }
        for name in [
            "nvidia-smi",
            "nvidia_icd.json",
            "nvhost_nvdec050_desc_prod.bin",
            "nvgstplayer-1.0_README.txt",
            "imx219.nito",
            ".so",
            "libfoo.so.",
            "libfoo.so.1.",
            "libfoo.so.bak",
            "libfoo.so.1a",
        ] {
            assert!(!is_shared_library_file_name(name), "{name}");
        }
    }

    #[test]
    fn library_names_come_from_file_and_symlink_entries_once_each_in_order() {
        let entries = [
            entry(EntryKind::Device, "/dev/nvmap"),
            entry(
                EntryKind::File,
                "/usr/lib/aarch64-linux-gnu/nvidia/libnvcucompat.so",
            ),
            entry(EntryKind::File, "/usr/sbin/nvidia-smi"),
            entry(
                EntryKind::Symlink,
                "/usr/share/glvnd/egl_vendor.d/10_nvidia.json",
            ),
            entry(
                EntryKind::Symlink,
                "/usr/lib/aarch64-linux-gnu/libnvcucompat.so",
            ),
            entry(EntryKind::Directory, "/usr/lib/libstrange.so"),
            entry(
                EntryKind::Symlink,
                "/usr/lib/aarch64-linux-gnu/nvidia/libnvidia-ptxjitcompiler.so.1",
            ),
            entry(
                EntryKind::File,
                "/opt/nvidia/l4t-gpu-libs/openrm/libcuda.so.1.1",
            ),
        ];
        assert_eq!(
            shared_library_names(&entries),
            names(&[
                "libnvcucompat.so",
                "libnvidia-ptxjitcompiler.so.1",
                "libcuda.so.1.1"
            ])
        );
    }

    // -- the peppy block --------------------------------------------------------

    #[test]
    fn the_block_goes_after_the_stock_list_and_leaves_it_as_it_is() {
        let list = with_l4t_block(STOCK_LIST, &names(&["libnvos.so", "libnvcucompat.so"]));
        assert_eq!(
            list,
            format!("{STOCK_LIST}\n{BLOCK_BEGIN}\nlibnvos.so\nlibnvcucompat.so\n{BLOCK_END}\n")
        );
    }

    #[test]
    fn writing_the_same_names_again_changes_nothing() {
        let names = names(&["libnvos.so"]);
        let once = with_l4t_block(STOCK_LIST, &names);
        assert_eq!(with_l4t_block(&once, &names), once);
    }

    #[test]
    fn new_names_replace_the_block_and_no_names_remove_it() {
        let old = with_l4t_block(STOCK_LIST, &names(&["libnvos.so", "libnvgone.so"]));

        let new = with_l4t_block(&old, &names(&["libnvos.so", "libnvnew.so"]));
        assert_eq!(
            new,
            with_l4t_block(STOCK_LIST, &names(&["libnvos.so", "libnvnew.so"]))
        );

        assert_eq!(with_l4t_block(&old, &[]), STOCK_LIST);
    }

    #[test]
    fn lines_written_after_the_block_are_kept_and_the_block_moves_to_the_end() {
        let list = format!("{STOCK_LIST}\n{BLOCK_BEGIN}\nlibnvgone.so\n{BLOCK_END}\nlibmine.so\n");
        assert_eq!(
            with_l4t_block(&list, &names(&["libnvos.so"])),
            format!("{STOCK_LIST}\nlibmine.so\n\n{BLOCK_BEGIN}\nlibnvos.so\n{BLOCK_END}\n")
        );
    }

    #[test]
    fn a_list_without_a_block_given_no_names_is_returned_exactly() {
        for list in [
            STOCK_LIST,
            "libcuda.so",
            "libcuda.so\n\n\n",
            "libcuda.so  \n# end  \n\n",
            "",
        ] {
            assert_eq!(with_l4t_block(list, &[]), list, "for {list:?}");
        }
    }

    #[test]
    fn a_block_without_its_end_line_runs_to_the_end_of_the_file() {
        let list = format!("{STOCK_LIST}\n{BLOCK_BEGIN}\nlibnvgone.so\nlibnvhalf");
        assert_eq!(
            with_l4t_block(&list, &names(&["libnvos.so"])),
            with_l4t_block(STOCK_LIST, &names(&["libnvos.so"]))
        );
    }

    // -- sync -------------------------------------------------------------------

    /// An install tree holding the stock list, and a manifest directory that
    /// does not exist yet.
    struct Host {
        _scratch: TempDir,
        manifest_dir: PathBuf,
        nvliblist: PathBuf,
    }

    impl Host {
        fn new() -> Self {
            let scratch = scratch();
            let nvliblist = scratch.path().join("apptainer").join(NVLIBLIST_PATH);
            fs::create_dir_all(nvliblist.parent().unwrap()).unwrap();
            fs::write(&nvliblist, STOCK_LIST).unwrap();
            fs::set_permissions(&nvliblist, fs::Permissions::from_mode(0o644)).unwrap();
            Self {
                manifest_dir: scratch.path().join("host-files-for-container.d"),
                nvliblist,
                _scratch: scratch,
            }
        }

        fn write_manifest(&self, file: &str, text: &str) {
            fs::create_dir_all(&self.manifest_dir).unwrap();
            fs::write(self.manifest_dir.join(file), text).unwrap();
        }

        fn sync(&self) -> Result<NvliblistSync> {
            sync_nvliblist(&self.manifest_dir, &self.nvliblist)
        }

        fn list(&self) -> String {
            fs::read_to_string(&self.nvliblist).unwrap()
        }

        /// The inode of the list: a write replaces the file, so a new inode
        /// shows that the list was written.
        fn list_inode(&self) -> u64 {
            fs::metadata(&self.nvliblist).unwrap().ino()
        }
    }

    #[test]
    fn a_host_without_the_manifest_leaves_the_list_untouched() {
        let host = Host::new();
        let before = host.list_inode();

        assert_eq!(host.sync().unwrap(), NvliblistSync::NoManifest);

        assert_eq!(host.list(), STOCK_LIST);
        assert_eq!(host.list_inode(), before, "the list was written");
    }

    #[test]
    fn a_file_in_place_of_the_manifest_dir_is_no_manifest() {
        let host = Host::new();
        fs::create_dir_all(host.manifest_dir.parent().unwrap()).unwrap();
        fs::write(&host.manifest_dir, "not a directory").unwrap();

        assert_eq!(host.sync().unwrap(), NvliblistSync::NoManifest);
        assert_eq!(host.list(), STOCK_LIST);
    }

    /// The manifest directory exists and names no shared library: the list is
    /// not written, so an apptainer install the daemon cannot write still
    /// runs `--nv` commands. The stock list here ends in blank lines, which a
    /// rewrite would drop.
    #[test]
    fn a_manifest_without_shared_libraries_does_not_write_the_list() {
        for manifest in [None, Some("dev, /dev/nvidia0\nlib, /usr/bin/nvidia-smi\n")] {
            let host = Host::new();
            let stock_with_blank_lines = format!("{STOCK_LIST}\n\n");
            fs::write(&host.nvliblist, &stock_with_blank_lines).unwrap();
            fs::create_dir_all(&host.manifest_dir).unwrap();
            if let Some(text) = manifest {
                host.write_manifest("devices.csv", text);
            }
            let before = host.list_inode();

            assert_eq!(
                host.sync().unwrap(),
                NvliblistSync::Unchanged,
                "{manifest:?}"
            );

            assert_eq!(host.list(), stock_with_blank_lines, "{manifest:?}");
            assert_eq!(
                host.list_inode(),
                before,
                "the list was written: {manifest:?}"
            );
        }
    }

    #[test]
    fn the_libraries_of_every_csv_file_reach_the_list_in_file_name_order() {
        let host = Host::new();
        host.write_manifest(
            "l4t.csv",
            "lib, /opt/nvidia/l4t-gpu-libs/openrm/libcuda.so.1.1\n",
        );
        host.write_manifest(
            "drivers.csv",
            "lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvcucompat.so\n\
             lib, /usr/sbin/nvidia-smi\n\
             sym, /usr/lib/aarch64-linux-gnu/libnvcuextend.so\n",
        );
        host.write_manifest("devices.csv", "dev, /dev/nvmap\n");
        host.write_manifest("README", "not a manifest, and not read\n");

        assert_eq!(
            host.sync().unwrap(),
            NvliblistSync::Updated { libraries: 3 }
        );
        assert_eq!(
            host.list(),
            with_l4t_block(
                STOCK_LIST,
                &names(&["libnvcucompat.so", "libnvcuextend.so", "libcuda.so.1.1"])
            )
        );
        let mode = fs::metadata(&host.nvliblist).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn a_list_that_is_up_to_date_is_not_written_again() {
        let host = Host::new();
        host.write_manifest("drivers.csv", "lib, /usr/lib/libnvos.so\n");
        assert_eq!(
            host.sync().unwrap(),
            NvliblistSync::Updated { libraries: 1 }
        );
        let before = host.list_inode();

        assert_eq!(host.sync().unwrap(), NvliblistSync::Unchanged);
        assert_eq!(host.list_inode(), before, "the list was written");
    }

    #[test]
    fn a_library_dropped_from_the_manifest_leaves_the_list() {
        let host = Host::new();
        host.write_manifest(
            "drivers.csv",
            "lib, /usr/lib/libnvos.so\nlib, /usr/lib/libnvold.so\n",
        );
        host.sync().unwrap();

        host.write_manifest("drivers.csv", "lib, /usr/lib/libnvos.so\n");
        assert_eq!(
            host.sync().unwrap(),
            NvliblistSync::Updated { libraries: 1 }
        );
        assert_eq!(
            host.list(),
            with_l4t_block(STOCK_LIST, &names(&["libnvos.so"]))
        );
    }

    #[test]
    fn an_invalid_manifest_fails_and_leaves_the_list_untouched() {
        let host = Host::new();
        host.write_manifest("drivers.csv", "lib, /usr/lib/libnvos.so\nmount, /x\n");

        let error = host.sync().unwrap_err();
        assert!(
            matches!(&error, Error::L4tManifestInvalid { line: 2, .. }),
            "{error:?}"
        );
        assert_eq!(host.list(), STOCK_LIST);
    }

    #[test]
    fn a_list_that_cannot_be_read_fails_naming_it() {
        let host = Host::new();
        host.write_manifest("drivers.csv", "lib, /usr/lib/libnvos.so\n");
        fs::remove_file(&host.nvliblist).unwrap();

        let error = host.sync().unwrap_err();
        let Error::NvliblistUpdateFailed { path, .. } = &error else {
            panic!("expected NvliblistUpdateFailed, got {error:?}");
        };
        assert_eq!(path, &host.nvliblist.display().to_string());
    }

    /// The manifest of the Jetson Thor this module was written on (L4T R39.2):
    /// its lines of each kind, with the libraries whose absence crashed the
    /// first CUDA allocation in a container.
    #[test]
    fn a_real_l4t_manifest_gives_the_libraries_cuda_needs() {
        let text = "\
dev, /dev/nvhost-ctrl-pva0
dir, /dev/dri/by-path
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvcucompat.so
sym, /usr/lib/aarch64-linux-gnu/libnvcucompat.so
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvcuextend.so
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvrm_gpu.so
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvidia-rmapi-tegra.so.595.78
lib, /usr/lib/aarch64-linux-gnu/nvidia/libnvidia-ptxjitcompiler.so.595.78
sym, /usr/lib/aarch64-linux-gnu/nvidia/libnvidia-ptxjitcompiler.so.1
sym, /usr/share/glvnd/egl_vendor.d/10_nvidia.json
lib, /lib/firmware/tegra23x/nvhost_nvdec050_desc_prod.bin
lib, /usr/share/doc/package_name/LICENSE.nvidia-smi
lib, /opt/nvidia/l4t-gpu-libs/openrm/libcuda.so.1.1
";
        let entries = parse_manifest(text, Path::new("drivers.csv")).unwrap();
        assert_eq!(
            shared_library_names(&entries),
            names(&[
                "libnvcucompat.so",
                "libnvcuextend.so",
                "libnvrm_gpu.so",
                "libnvidia-rmapi-tegra.so.595.78",
                "libnvidia-ptxjitcompiler.so.595.78",
                "libnvidia-ptxjitcompiler.so.1",
                "libcuda.so.1.1",
            ])
        );
    }
}
