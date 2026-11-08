//! sha256 digests of bytes and files, as [`OciDigest`]s.

use core_node_api::encoding::OciDigest;
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::Path;

/// The size of the reads that hash a file.
const READ_BUFFER_BYTES: usize = 1 << 20;

/// The digest of `bytes`.
pub fn of_bytes(bytes: &[u8]) -> OciDigest {
    finish(Sha256::digest(bytes).as_slice())
}

/// The digest and the size of the file at `path`. Blocking: run it on the
/// blocking pool.
pub fn of_file(path: &Path) -> std::io::Result<(OciDigest, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((finish(hasher.finalize().as_slice()), size))
}

/// An incremental sha256, for bytes that arrive in parts.
#[derive(Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(self) -> OciDigest {
        finish(self.0.finalize().as_slice())
    }
}

fn finish(hash: &[u8]) -> OciDigest {
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    OciDigest::from_sha256_hex(&hex).expect("a sha256 is 64 hexadecimal characters")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_of_bytes_is_their_sha256() {
        assert_eq!(
            of_bytes(b"{}").as_str(),
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn a_file_and_its_bytes_in_parts_have_the_digest_of_the_whole() {
        let bytes: Vec<u8> = (0..3 * READ_BUFFER_BYTES + 17).map(|i| i as u8).collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blob");
        std::fs::write(&path, &bytes).expect("write");

        let (digest, size) = of_file(&path).expect("hash");

        assert_eq!(digest, of_bytes(&bytes));
        assert_eq!(size, bytes.len() as u64);
        let mut hasher = Hasher::default();
        for part in bytes.chunks(1000) {
            hasher.update(part);
        }
        assert_eq!(hasher.finish(), digest);
    }
}
