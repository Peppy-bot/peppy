//! Archive extraction primitives shared by node-stack and core-node.
//!
//! Lives at the crate root (rather than under `node_stack::start_steps`)
//! because the same `.tar.zst` format is used by two unrelated callers:
//! the start lifecycle's process-node archive extraction, and core-node's
//! node-add source resolution. Keeping this here avoids the start pipeline
//! "owning" a helper that has nothing to do with the start lifecycle.

use std::path::{Component, Path};
use tar::Archive;
use zstd::stream::read::Decoder;

/// Extracts a `.tar.zst` archive into `destination` with path safety checks.
/// Rejects entries containing `..`, root, or prefix path components.
/// Directories are applied last to avoid permission interference during extraction.
pub fn extract_tar_zst(archive_path: &Path, destination: &Path) -> std::result::Result<(), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {}", archive_path.display(), e))?;

    let decoder = Decoder::new(file).map_err(|e| {
        format!(
            "Failed to decode zstd archive {}: {}",
            archive_path.display(),
            e
        )
    })?;
    let mut archive = Archive::new(decoder);

    let entries = archive.entries().map_err(|e| {
        format!(
            "Failed to read archive entries from {}: {}",
            archive_path.display(),
            e
        )
    })?;

    let mut directories = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            format!(
                "Failed to read archive entry from {}: {}",
                archive_path.display(),
                e
            )
        })?;

        let entry_path = entry
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }

        if entry.header().entry_type().is_dir() {
            directories.push(entry);
        } else {
            let unpacked = entry.unpack_in(destination).map_err(|e| {
                format!(
                    "Failed to unpack entry {} from {}: {}",
                    entry_path.display(),
                    archive_path.display(),
                    e
                )
            })?;
            if !unpacked {
                return Err(format!(
                    "Archive {} contains unsafe path: {}",
                    archive_path.display(),
                    entry_path.display()
                ));
            }
        }
    }

    // Apply directory entries at the end, matching tar::Archive::unpack behavior (avoids
    // directory permissions interfering with descendant extraction).
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        let entry_path = dir
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();
        let unpacked = dir.unpack_in(destination).map_err(|e| {
            format!(
                "Failed to unpack entry {} from {}: {}",
                entry_path.display(),
                archive_path.display(),
                e
            )
        })?;
        if !unpacked {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        header[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());

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

        let err = extract_tar_zst(&archive, &dest).expect_err("traversal must be rejected");
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

        let err = extract_tar_zst(&archive, &dest).expect_err("absolute path must be rejected");
        assert!(
            err.contains("unsafe path"),
            "expected an unsafe-path rejection, got: {err}"
        );
    }

    #[test]
    fn extracts_safe_nested_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive = tmp.path().join("safe.tar.zst");
        write_archive(&archive, &raw_tar("sub/dir/file.txt", b"hello"));
        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).expect("mkdir dest");

        extract_tar_zst(&archive, &dest).expect("a safe archive should extract");
        let extracted = dest.join("sub").join("dir").join("file.txt");
        assert_eq!(
            std::fs::read_to_string(&extracted).expect("read extracted file"),
            "hello",
        );
    }
}
