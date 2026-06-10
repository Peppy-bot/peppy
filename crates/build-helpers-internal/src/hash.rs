//! SHA-256 hashing and verification of downloaded artifacts.

use std::io::Read;
use std::path::Path;

/// Computes the SHA-256 hash of a file using the `sha2` crate. Returns `None` on I/O error.
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            println!(
                "cargo:warning=Failed to open {:?} for SHA-256 verification: {}",
                path, e
            );
            return None;
        }
    };

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let n = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(e) => {
                println!(
                    "cargo:warning=Failed to read {:?} for SHA-256 verification: {}",
                    path, e
                );
                return None;
            }
        };
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    let mut hex = String::with_capacity(hash.len() * 2);
    for byte in hash {
        use std::fmt::Write;
        write!(hex, "{:02x}", byte).unwrap();
    }
    Some(hex)
}

/// Verifies the SHA-256 hash of a file against an expected value.
/// Returns `true` if the hash matches.
pub fn verify_sha256(path: &Path, expected: &str, label: &str) -> bool {
    let Some(actual) = sha256_file(path) else {
        return false;
    };

    if actual.eq_ignore_ascii_case(expected) {
        true
    } else {
        println!(
            "cargo:warning={} SHA-256 mismatch for {:?}: expected {}, got {}",
            label, path, expected, actual
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of b"abc" (FIPS 180-2 test vector).
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn temp_file_with(contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, contents).expect("write artifact");
        (dir, path)
    }

    #[test]
    fn verify_sha256_accepts_matching_hash() {
        let (_dir, path) = temp_file_with(b"abc");
        assert!(verify_sha256(&path, ABC_SHA256, "test"));
    }

    #[test]
    fn verify_sha256_is_case_insensitive() {
        let (_dir, path) = temp_file_with(b"abc");
        assert!(verify_sha256(&path, &ABC_SHA256.to_uppercase(), "test"));
    }

    #[test]
    fn verify_sha256_rejects_mismatching_hash() {
        let (_dir, path) = temp_file_with(b"abc");
        assert!(!verify_sha256(&path, &"0".repeat(64), "test"));
    }

    #[test]
    fn verify_sha256_rejects_missing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(!verify_sha256(
            &dir.path().join("no-such-file"),
            ABC_SHA256,
            "test"
        ));
    }

    #[test]
    fn verify_sha256_hashes_files_larger_than_the_read_buffer() {
        // 200,000 bytes exceeds the 64 KiB read buffer, forcing the read
        // loop through multiple iterations.
        let (_dir, path) = temp_file_with(&vec![b'a'; 200_000]);
        assert!(verify_sha256(
            &path,
            "2287d207f24a941ff3b56c04c8a25ad56b63e3023207b3bb5b4ac0c9869d74be",
            "test"
        ));
    }
}
