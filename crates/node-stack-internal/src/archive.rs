//! Extraction of tar archives under path safety rules: the `.tar.zst`
//! archive of a node, shared by the run steps of process nodes
//! (`node_stack::run_steps`) and by core-node's node-add source resolution,
//! and the release archive of peppy that `peppy update` installs.

use std::io::Read;
use std::path::{Component, Path};
use tar::{Archive, EntryType};
use zstd::stream::read::Decoder;

/// The most bytes that the archive of a node source unpacks to (the tar
/// stream inside the zstd frame). A source bundle is far smaller, and a
/// compressed archive of some kilobytes can unpack to terabytes.
pub const MAX_NODE_SOURCE_UNPACKED_BYTES: u64 = 16 << 30;

/// The most bytes that the release archive of peppy unpacks to (the tar
/// stream inside the gzip stream). A release unpacks to some gigabytes at
/// most.
pub const MAX_PEPPY_RELEASE_UNPACKED_BYTES: u64 = 16 << 30;

/// The directory at the root of the release archive of peppy that holds
/// each entry of the archive.
const PEPPY_RELEASE_ROOT_DIR: &str = "bin";

/// Where an archive comes from, which decides what it may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOrigin {
    /// The archive that this machine built from a node (`built_nodes/`). It
    /// holds what the build left, symbolic links to the host included (the
    /// interpreter of a Python environment).
    BuiltHere,
    /// The archive of a node source that `peppy node add` reads: a local
    /// `.tar.zst` or a download. It holds regular files, directories, and
    /// symbolic links whose target is a relative path with no `..` (so the
    /// link stays in the archive), and it unpacks to at most
    /// [`MAX_NODE_SOURCE_UNPACKED_BYTES`].
    NodeSource,
    /// The release archive of peppy that `peppy update` downloads. It holds
    /// `bin/` and what is under it alone: regular files, directories, and
    /// symbolic links whose target is a relative path with no `..` (so the
    /// link stays in `bin/` after the installation), and it unpacks to at
    /// most [`MAX_PEPPY_RELEASE_UNPACKED_BYTES`].
    PeppyRelease,
}

impl ArchiveOrigin {
    const fn max_unpacked_bytes(self) -> u64 {
        match self {
            Self::BuiltHere => u64::MAX,
            Self::NodeSource => MAX_NODE_SOURCE_UNPACKED_BYTES,
            Self::PeppyRelease => MAX_PEPPY_RELEASE_UNPACKED_BYTES,
        }
    }

    /// What an archive of this origin holds, for the error that refuses an
    /// entry, or `None` when the archive can hold each entry.
    const fn allowed_content(self) -> Option<&'static str> {
        match self {
            Self::BuiltHere => None,
            Self::NodeSource => Some(
                "the archive of a node source holds files, directories, and symbolic links to a \
                 relative path in it with no `..`",
            ),
            Self::PeppyRelease => Some(
                "the release archive of peppy holds `bin/` alone, with files, directories, and \
                 symbolic links to a relative path in it with no `..`",
            ),
        }
    }
}

/// Extracts a `.tar.zst` archive into `destination` with path safety checks.
/// Rejects entries containing `..`, root, or prefix path components, and
/// the entries that `origin` does not allow.
/// Directories are applied last to avoid permission interference during extraction.
pub fn extract_tar_zst(
    archive_path: &Path,
    destination: &Path,
    origin: ArchiveOrigin,
) -> Result<(), String> {
    let archive_name = archive_path.display().to_string();
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {archive_name}: {e}"))?;

    let decoder = Decoder::new(file)
        .map_err(|e| format!("Failed to decode zstd archive {archive_name}: {e}"))?;
    extract_tar(decoder, &archive_name, destination, origin)
}

/// Extracts the tar stream `tar_stream`, which is already decompressed, into
/// `destination` with path safety checks. `archive_name` names the archive in
/// the errors.
/// Rejects entries containing `..`, root, or prefix path components, the
/// entries that `origin` does not allow, and a stream that unpacks to more
/// than the bound of `origin`.
/// Directories are applied last to avoid permission interference during extraction.
pub fn extract_tar(
    tar_stream: impl Read,
    archive_name: &str,
    destination: &Path,
    origin: ArchiveOrigin,
) -> Result<(), String> {
    let mut archive = Archive::new(BoundedReader {
        inner: tar_stream,
        read_bytes: 0,
        max_bytes: origin.max_unpacked_bytes(),
    });

    let entries = archive
        .entries()
        .map_err(|e| format!("Failed to read archive entries from {archive_name}: {e}"))?;

    let mut directories = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("Failed to read archive entry from {archive_name}: {e}"))?;

        let entry_path = entry
            .path()
            .map_err(|e| format!("Failed to read entry path from {archive_name}: {e}"))?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(unsafe_path_error(archive_name, &entry_path));
        }
        if let Some(allowed_content) = origin.allowed_content() {
            refuse_a_foreign_entry(&entry, &entry_path, origin, archive_name, allowed_content)?;
        }

        if entry.header().entry_type().is_dir() {
            directories.push((entry_path, entry));
            continue;
        }
        unpack_entry(entry, &entry_path, destination, archive_name)?;
    }

    // Apply directory entries at the end, matching tar::Archive::unpack behavior (avoids
    // directory permissions interfering with descendant extraction).
    directories.sort_by(|(_, a), (_, b)| b.path_bytes().cmp(&a.path_bytes()));
    for (entry_path, dir) in directories {
        unpack_entry(dir, &entry_path, destination, archive_name)?;
    }

    Ok(())
}

/// Unpacks `entry`, whose path is `entry_path`, under `destination`.
/// `unpack_in` refuses an entry whose path would leave `destination`, which
/// is an unsafe path too.
fn unpack_entry<R: Read>(
    mut entry: tar::Entry<'_, R>,
    entry_path: &Path,
    destination: &Path,
    archive_name: &str,
) -> Result<(), String> {
    let unpacked = entry.unpack_in(destination).map_err(|e| {
        format!(
            "Failed to unpack entry {} from {archive_name}: {e}",
            entry_path.display()
        )
    })?;
    if !unpacked {
        return Err(unsafe_path_error(archive_name, entry_path));
    }
    Ok(())
}

/// Refuses an entry that an archive of `origin`, whose content
/// `allowed_content` describes, cannot hold: a hard link, a device, a FIFO,
/// or a symbolic link whose target can leave the archive (an absolute path,
/// or a path with a `..`). The release archive of peppy also cannot hold an
/// entry outside `bin/`.
fn refuse_a_foreign_entry<R: Read>(
    entry: &tar::Entry<'_, R>,
    entry_path: &Path,
    origin: ArchiveOrigin,
    archive_name: &str,
    allowed_content: &str,
) -> Result<(), String> {
    let refuse = |what: String| {
        Err(format!(
            "Archive {archive_name} holds {what} at {}: {allowed_content}",
            entry_path.display()
        ))
    };
    if origin == ArchiveOrigin::PeppyRelease && !is_in_peppy_release_root(entry_path) {
        return refuse("an entry outside `bin/`".to_owned());
    }
    match entry.header().entry_type() {
        EntryType::Regular | EntryType::Continuous | EntryType::Directory => Ok(()),
        EntryType::Symlink => {
            let target = entry
                .link_name()
                .map_err(|e| format!("Failed to read a link of {archive_name}: {e}"))?
                .unwrap_or_default();
            let stays_in_the_archive = target.is_relative()
                && target
                    .components()
                    .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
            if stays_in_the_archive {
                Ok(())
            } else {
                refuse(format!("a symbolic link to {}", target.display()))
            }
        }
        EntryType::Link => refuse("a hard link".to_owned()),
        EntryType::Char | EntryType::Block => refuse("a device".to_owned()),
        EntryType::Fifo => refuse("a FIFO".to_owned()),
        other => refuse(format!("an entry of type {other:?}")),
    }
}

/// Whether `entry_path` is `bin/` or under it, or names the root of the
/// archive itself (`./`), which unpacks to nothing.
fn is_in_peppy_release_root(entry_path: &Path) -> bool {
    entry_path
        .components()
        .find_map(|component| match component {
            Component::Normal(name) => Some(name == PEPPY_RELEASE_ROOT_DIR),
            _ => None,
        })
        .unwrap_or(true)
}

/// The decompressed stream of an archive, which fails once it gives more
/// than `max_bytes`.
struct BoundedReader<R> {
    inner: R,
    read_bytes: u64,
    max_bytes: u64,
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.read_bytes = self.read_bytes.saturating_add(read as u64);
        if self.read_bytes > self.max_bytes {
            return Err(std::io::Error::other(format!(
                "the archive unpacks to more than {} bytes",
                self.max_bytes
            )));
        }
        Ok(read)
    }
}

fn unsafe_path_error(archive_name: &str, entry_path: &Path) -> String {
    format!(
        "Archive {archive_name} contains unsafe path: {}",
        entry_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use zstd::stream::write::Encoder;

    /// Writes a NUL-terminated octal field (the USTAR numeric encoding).
    fn write_octal(field: &mut [u8], value: u64) {
        let digits = format!("{:0width$o}", value, width = field.len() - 1);
        field[..digits.len()].copy_from_slice(digits.as_bytes());
    }

    /// Encodes a single regular-file USTAR entry, writing `name` straight into
    /// the raw 100-byte name field. This bypasses `tar::Builder`, whose
    /// `set_path` refuses a `..` component, so the extractor's own traversal
    /// guard is what gets exercised. Returns the uncompressed tar bytes.
    fn raw_tar(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut header[100..108], 0o644); // mode
        write_octal(&mut header[108..116], 0); // uid
        write_octal(&mut header[116..124], 0); // gid
        write_octal(&mut header[124..136], data.len() as u64); // size
        write_octal(&mut header[136..148], 0); // mtime
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // Checksum is computed with the checksum field read as ASCII spaces.
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            out.resize(out.len() + (512 - rem), 0);
        }
        // Two zero blocks mark end-of-archive.
        out.resize(out.len() + 1024, 0);
        out
    }

    /// Compresses `tar_bytes` to a `.tar.zst` file at `archive_path`.
    fn write_archive(archive_path: &Path, tar_bytes: &[u8]) {
        let file = std::fs::File::create(archive_path).expect("create archive");
        let mut encoder = Encoder::new(file, 1).expect("zstd encoder");
        std::io::Write::write_all(&mut encoder, tar_bytes).expect("write tar bytes");
        encoder.finish().expect("finish zstd");
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("evil.tar.zst");
        write_archive(&archive, &raw_tar("../escape.txt", b"pwned"));
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");

        let err = extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource)
            .expect_err("traversal must be rejected");
        assert!(
            err.contains("unsafe path"),
            "expected an unsafe-path rejection, got: {err}"
        );
        // The traversal target sits one level above `dest`; it must not exist.
        assert!(
            !tmp.path().join("escape.txt").exists(),
            "traversal entry must not be written outside the destination"
        );
    }

    #[test]
    fn rejects_absolute_path_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("abs.tar.zst");
        write_archive(&archive, &raw_tar("/abs/escape.txt", b"pwned"));
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");

        let err = extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource)
            .expect_err("absolute path must be rejected");
        assert!(
            err.contains("unsafe path"),
            "expected an unsafe-path rejection, got: {err}"
        );
    }

    /// A directory entry is applied after the files under it, which still
    /// land inside it.
    #[test]
    fn extracts_a_directory_entry_and_its_file() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_entry_type(EntryType::Directory);
        dir_header.set_mode(0o755);
        dir_header.set_size(0);
        builder
            .append_data(&mut dir_header, "sub/", std::io::empty())
            .expect("append the directory entry");
        let mut file_header = tar::Header::new_gnu();
        file_header.set_mode(0o644);
        file_header.set_size(5);
        builder
            .append_data(&mut file_header, "sub/file.txt", &b"hello"[..])
            .expect("append the file entry");
        let tar_bytes = builder.into_inner().expect("finish the tar");

        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("dir.tar.zst");
        write_archive(&archive, &tar_bytes);
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");

        extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource)
            .expect("a safe archive should extract");
        assert!(
            dest.join("sub").is_dir(),
            "the directory entry is a directory"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("sub").join("file.txt"))
                .expect("read extracted file"),
            "hello",
        );
    }

    /// A `.tar.zst` at `<tmp>/archive.tar.zst` with one entry of `kind` at
    /// `path` (a link to `target` for a link), and an empty `<tmp>/dest`.
    fn archive_with_one_entry(
        tmp: &Path,
        kind: EntryType,
        path: &str,
        target: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(0);
        match target {
            Some(target) => builder
                .append_link(&mut header, path, target)
                .expect("append the link"),
            None => builder
                .append_data(&mut header, path, std::io::empty())
                .expect("append the entry"),
        }
        let archive = tmp.join("archive.tar.zst");
        write_archive(&archive, &builder.into_inner().expect("finish the tar"));
        let dest = tmp.join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");
        (archive, dest)
    }

    #[test]
    fn a_node_source_refuses_a_link_that_can_leave_it_or_a_special_file() {
        for (kind, path, target) in [
            (
                EntryType::Symlink,
                "peppy.json5",
                Some("/home/user/.peppy/conf/credentials.json5"),
            ),
            (EntryType::Symlink, "data", Some("../../outside")),
            (EntryType::Symlink, "sub/data", Some("../sub2/file")),
            (EntryType::Link, "peppy.json5", Some("other.json5")),
            (EntryType::Fifo, "pipe", None),
            (EntryType::Char, "tty", None),
            (EntryType::Block, "disk", None),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (archive, dest) = archive_with_one_entry(tmp.path(), kind, path, target);

            let error = extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource)
                .expect_err("the entry is refused");

            assert!(
                error.contains("the archive of a node source holds"),
                "{kind:?}: {error}"
            );
            assert!(
                std::fs::symlink_metadata(dest.join(path)).is_err(),
                "{kind:?}: the entry is not written"
            );
        }
    }

    #[test]
    fn a_node_source_keeps_a_link_down_inside_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (archive, dest) = archive_with_one_entry(
            tmp.path(),
            EntryType::Symlink,
            "lib.so",
            Some("./sub/lib.so.1"),
        );

        extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource).expect("a link inside");

        assert_eq!(
            std::fs::read_link(dest.join("lib.so")).expect("a link"),
            Path::new("./sub/lib.so.1")
        );
    }

    /// The archive that this machine built keeps the links of its build,
    /// such as the interpreter of a Python environment.
    #[test]
    fn an_archive_built_here_keeps_a_link_to_the_host() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (archive, dest) = archive_with_one_entry(
            tmp.path(),
            EntryType::Symlink,
            "python",
            Some("/usr/bin/python3"),
        );

        extract_tar_zst(&archive, &dest, ArchiveOrigin::BuiltHere).expect("a built archive");

        assert_eq!(
            std::fs::read_link(dest.join("python")).expect("a link"),
            Path::new("/usr/bin/python3")
        );
    }

    /// The tar bytes of one entry of `kind` at `path` (a link to `target`
    /// for a link).
    fn tar_with_one_entry(kind: EntryType, path: &str, target: Option<&str>) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(0);
        match target {
            Some(target) => builder
                .append_link(&mut header, path, target)
                .expect("append the link"),
            None => builder
                .append_data(&mut header, path, std::io::empty())
                .expect("append the entry"),
        }
        builder.into_inner().expect("finish the tar")
    }

    #[test]
    fn a_peppy_release_refuses_an_entry_outside_bin_or_a_link_that_can_leave_bin() {
        for (kind, path, target, refused) in [
            (
                EntryType::Regular,
                "./README",
                None,
                "an entry outside `bin/`",
            ),
            (
                EntryType::Directory,
                "./share/",
                None,
                "an entry outside `bin/`",
            ),
            (
                EntryType::Symlink,
                "./bin/apptainer/data",
                Some("../.."),
                "a symbolic link to ../..",
            ),
            (
                EntryType::Symlink,
                "./bin/peppy",
                Some("/usr/bin/peppy"),
                "a symbolic link to /usr/bin/peppy",
            ),
            (
                EntryType::Link,
                "./bin/zenohd",
                Some("bin/peppy"),
                "a hard link",
            ),
            (EntryType::Fifo, "./bin/pipe", None, "a FIFO"),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");

            let error = extract_tar(
                tar_with_one_entry(kind, path, target).as_slice(),
                "peppy.tgz",
                tmp.path(),
                ArchiveOrigin::PeppyRelease,
            )
            .expect_err("the entry is refused");

            assert!(error.contains(refused), "{kind:?} {path}: {error}");
            assert!(
                error.contains("the release archive of peppy holds `bin/` alone"),
                "{kind:?} {path}: {error}"
            );
            assert!(
                std::fs::symlink_metadata(tmp.path().join(path)).is_err(),
                "{kind:?} {path}: the entry is not written"
            );
        }
    }

    /// A release archive lists `./bin` before what is under it, as Python's
    /// `tarfile.add` writes it. A directory that its owner cannot write
    /// still gets its files, because directories are applied last.
    #[test]
    fn a_peppy_release_unpacks_bin_with_a_read_only_directory_and_a_link_down_inside_it() {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, mode) in [("./bin/", 0o755), ("./bin/apptainer/", 0o555)] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(EntryType::Directory);
            header.set_mode(mode);
            header.set_size(0);
            builder
                .append_data(&mut header, path, std::io::empty())
                .expect("append the directory entry");
        }
        let mut file_header = tar::Header::new_gnu();
        file_header.set_mode(0o755);
        file_header.set_size(3);
        builder
            .append_data(&mut file_header, "./bin/apptainer/run", &b"run"[..])
            .expect("append the file entry");
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(EntryType::Symlink);
        link_header.set_mode(0o777);
        link_header.set_size(0);
        builder
            .append_link(&mut link_header, "./bin/apptainer/alias", "run")
            .expect("append the link");
        let tar_bytes = builder.into_inner().expect("finish the tar");
        let tmp = tempfile::tempdir().expect("tempdir");

        extract_tar(
            tar_bytes.as_slice(),
            "peppy.tgz",
            tmp.path(),
            ArchiveOrigin::PeppyRelease,
        )
        .expect("a release archive extracts");

        let apptainer = tmp.path().join("bin/apptainer");
        assert_eq!(
            std::fs::read_to_string(apptainer.join("run")).expect("read the file"),
            "run"
        );
        assert_eq!(
            std::fs::read_link(apptainer.join("alias")).expect("a link"),
            Path::new("run")
        );
        // The temporary directory removes its tree only when the owner can
        // write each directory of it.
        std::fs::set_permissions(&apptainer, std::fs::Permissions::from_mode(0o755))
            .expect("make the directory writable");
    }

    #[test]
    fn a_stream_longer_than_its_bound_fails() {
        let mut bounded = BoundedReader {
            inner: &[7u8; 100][..],
            read_bytes: 0,
            max_bytes: 64,
        };
        let error = bounded
            .read_to_end(&mut Vec::new())
            .expect_err("more than the bound");
        assert!(error.to_string().contains("more than 64 bytes"), "{error}");

        let mut exact = BoundedReader {
            inner: &[7u8; 64][..],
            read_bytes: 0,
            max_bytes: 64,
        };
        let mut read = Vec::new();
        exact.read_to_end(&mut read).expect("the bound itself");
        assert_eq!(read.len(), 64);
    }

    #[test]
    fn extracts_safe_nested_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("safe.tar.zst");
        write_archive(&archive, &raw_tar("sub/dir/file.txt", b"hello"));
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");

        extract_tar_zst(&archive, &dest, ArchiveOrigin::NodeSource)
            .expect("a safe archive should extract");
        let extracted = dest.join("sub").join("dir").join("file.txt");
        assert_eq!(
            std::fs::read_to_string(&extracted).expect("read extracted file"),
            "hello",
        );
    }
}
