//! Filesystem helpers: change-aware writes.

use std::path::Path;

/// Write `contents` to `path` only if the file does not already contain identical data.
///
/// Avoids bumping the file's mtime when content is unchanged, which prevents
/// cargo from detecting a spurious change via `rerun-if-changed` or
/// `include!`/`include_bytes!` tracking.
///
/// Returns `true` if the file was actually written (content changed or file was new).
pub fn write_if_changed(path: &Path, contents: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|existing| existing == contents) {
        return false;
    }
    std::fs::write(path, contents).unwrap_or_else(|e| {
        panic!("Failed to write {}: {}", path.display(), e);
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_if_changed_writes_new_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        assert!(write_if_changed(&path, b"hello"));
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn write_if_changed_skips_identical_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"hello").expect("write");
        assert!(!write_if_changed(&path, b"hello"));
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
    }

    #[test]
    fn write_if_changed_overwrites_different_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"hello").expect("write");
        assert!(write_if_changed(&path, b"world"));
        assert_eq!(std::fs::read(&path).expect("read"), b"world");
    }

    #[test]
    fn write_if_changed_truncates_to_empty_contents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"nonempty").expect("write");
        assert!(write_if_changed(&path, b""));
        assert_eq!(std::fs::read(&path).expect("read"), b"");
    }
}
