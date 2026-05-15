//! Shared name/tag validation for repo-backed node identifiers.
//!
//! Returning `Result<(), String>` lets each caller wrap the detail into its
//! own error type.

use crate::consts::ALLOWED_CONFIG_CHARS;
use crate::internal::node::Name;

pub fn validate_repo_node_name(value: &str, label: &str) -> Result<(), String> {
    Name::try_from(value.to_owned())
        .map(|_| ())
        .map_err(|e| format!("invalid {label}: {e}"))
}

pub fn validate_repo_node_tag(tag: &str, label: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Err(format!("empty {label}"));
    }
    if !tag.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(format!("{label} must start with an ASCII letter: {tag}"));
    }
    for c in tag.chars() {
        if !ALLOWED_CONFIG_CHARS.contains(c) {
            return Err(format!(
                "{label} contains disallowed character {c:?}: {tag} \
                 (allowed: {ALLOWED_CONFIG_CHARS})"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_name() {
        assert!(validate_repo_node_name("robot_brain", "name").is_ok());
        assert!(validate_repo_node_name("Camera-V2", "name").is_ok());
    }

    #[test]
    fn rejects_empty_name() {
        assert!(validate_repo_node_name("", "name").is_err());
    }

    #[test]
    fn rejects_name_with_slash() {
        assert!(validate_repo_node_name("foo/bar", "name").is_err());
    }

    #[test]
    fn accepts_valid_tag() {
        assert!(validate_repo_node_tag("v1", "tag").is_ok());
        assert!(validate_repo_node_tag("donut", "tag").is_ok());
        assert!(validate_repo_node_tag("v2-rc1", "tag").is_ok());
        assert!(validate_repo_node_tag("My_Tag-9", "tag").is_ok());
    }

    #[test]
    fn rejects_empty_tag() {
        let err = validate_repo_node_tag("", "tag").unwrap_err();
        assert_eq!(err, "empty tag");
    }

    #[test]
    fn rejects_tag_not_starting_with_letter() {
        for tag in ["1", "1v", "0", "9donut", "_v1", "-v1"] {
            let err = validate_repo_node_tag(tag, "tag")
                .expect_err(&format!("expected `{tag}` to be rejected"));
            assert!(
                err.contains("must start with an ASCII letter"),
                "wrong error for `{tag}`: {err}"
            );
        }
    }

    #[test]
    fn rejects_dot_in_tag() {
        assert!(validate_repo_node_tag("v0.1.0", "tag").is_err());
        assert!(validate_repo_node_tag("v1.2", "tag").is_err());
        assert!(validate_repo_node_tag("v.hidden", "tag").is_err());
        assert!(validate_repo_node_tag("a..b", "tag").is_err());
    }

    #[test]
    fn rejects_disallowed_chars_in_tag() {
        assert!(validate_repo_node_tag("v1/2", "tag").is_err());
        assert!(validate_repo_node_tag("v1 beta", "tag").is_err());
        assert!(validate_repo_node_tag("v1+rc", "tag").is_err());
    }
}
