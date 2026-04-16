//! Shared name/tag validation for repo-backed node identifiers.
//!
//! Used both by the launch-file parser (where a deployment source can be
//! `{ name, tag }`) and by the node-add encoding (where `NodeSource::RepoNode`
//! carries the same identifiers). Returning `Result<(), String>` lets each
//! caller wrap the detail into its own error type.

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
    if tag == "." || tag == ".." || tag.starts_with('.') {
        return Err(format!("{label} must not start with '.': {tag}"));
    }
    if tag.contains("..") {
        return Err(format!("{label} must not contain '..': {tag}"));
    }
    for c in tag.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        if !ok {
            return Err(format!(
                "{label} contains disallowed character {c:?}: {tag}"
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
        assert!(validate_repo_node_tag("0.1.0", "tag").is_ok());
        assert!(validate_repo_node_tag("v1", "tag").is_ok());
        assert!(validate_repo_node_tag("1.2.3-rc1", "tag").is_ok());
    }

    #[test]
    fn rejects_empty_tag() {
        let err = validate_repo_node_tag("", "tag").unwrap_err();
        assert_eq!(err, "empty tag");
    }

    #[test]
    fn rejects_traversal_tag() {
        assert!(validate_repo_node_tag("..", "tag").is_err());
        assert!(validate_repo_node_tag("..something", "tag").is_err());
        assert!(validate_repo_node_tag(".hidden", "tag").is_err());
        assert!(validate_repo_node_tag("a..b", "tag").is_err());
    }

    #[test]
    fn rejects_disallowed_chars_in_tag() {
        assert!(validate_repo_node_tag("1.0/2", "tag").is_err());
        assert!(validate_repo_node_tag("1.0 beta", "tag").is_err());
    }
}
