//! Deterministic on-disk cache keys for the git checkout cache in
//! [`super::git`], which names its directories `<slug>-<hash>`: the slug is
//! a human-readable prefix derived from the repository URL, the hash
//! disambiguates the commits cached for that same URL.

use sha2::{Digest, Sha256};

/// Returns a short sanitized slug derived from `raw`. Keeps
/// `[a-zA-Z0-9._-]`, replaces every other character with `_`, and caps
/// length at 40. Returns `fallback` when the cleaned result is empty.
pub(super) fn slug(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let truncated: String = trimmed.chars().take(40).collect();
    if truncated.is_empty() {
        fallback.to_owned()
    } else {
        truncated
    }
}

/// Returns the first 16 hex chars of `sha256(url || '\0' || qualifier)`.
/// Used as a cache-key suffix so that two `qualifier`s (the commit, for
/// the git checkout cache) never collide on the same slug.
pub(super) fn short_hash(url: &str, qualifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.update([0u8]);
    hasher.update(qualifier.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_sanitizes_url_characters() {
        assert_eq!(
            slug("https://github.com/foo/bar.git", "repo"),
            "https___github.com_foo_bar.git"
        );
    }

    #[test]
    fn slug_returns_fallback_for_empty_cleaned_string() {
        assert_eq!(slug("", "repo"), "repo");
        assert_eq!(slug("////", "bundle"), "bundle");
    }

    #[test]
    fn short_hash_differs_on_qualifier() {
        let a = short_hash("https://example.com/repo.git", "v1");
        let b = short_hash("https://example.com/repo.git", "v2");
        assert_ne!(a, b);
    }

    #[test]
    fn short_hash_stable_for_same_input() {
        let a = short_hash("https://example.com/repo.git", "main");
        let b = short_hash("https://example.com/repo.git", "main");
        assert_eq!(a, b);
    }
}
