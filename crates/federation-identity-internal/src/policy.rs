use sha2::{Digest, Sha256};

const MAX_RENEWAL_JITTER_SECS: i64 = 5 * 60;

pub(crate) fn stable_renewal_jitter(generation: &str) -> i64 {
    let digest = Sha256::digest(generation.as_bytes());
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(prefix) % (MAX_RENEWAL_JITTER_SECS as u64 + 1)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_jitter_keeps_the_existing_stable_vector() {
        assert_eq!(stable_renewal_jitter(&"a".repeat(64)), 153);
        assert_eq!(stable_renewal_jitter("generation-1"), 269);
    }

    #[test]
    fn renewal_jitter_is_bounded_and_deterministic() {
        let first = stable_renewal_jitter("one-generation");
        let second = stable_renewal_jitter("one-generation");

        assert_eq!(first, second);
        assert!((0..=MAX_RENEWAL_JITTER_SECS).contains(&first));
    }
}
