use std::fmt;
use std::str::FromStr;

use config::Language;

use crate::{Error, Result};

/// A validated node name that ensures it follows naming conventions.
/// Node names must start with a letter and contain only alphanumeric characters,
/// underscores, or hyphens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeName(String);

impl NodeName {
    /// Creates a new NodeName after validating the input.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();

        if name.is_empty() {
            return Err(Error::InvalidNodeName(
                "Node name cannot be empty".to_string(),
            ));
        }

        // Check if the first character is a letter
        if !name.chars().next().unwrap().is_ascii_alphabetic() {
            return Err(Error::InvalidNodeName(format!(
                "Node name '{}' must start with a letter",
                name
            )));
        }

        // Check if all characters are alphanumeric, underscore, or hyphen
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::InvalidNodeName(format!(
                "Node name '{}' contains invalid characters",
                name
            )));
        }

        Ok(Self(name))
    }

    /// Returns the node name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for NodeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for NodeName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_name_validation() {
        // Valid names
        assert!(NodeName::new("my_node").is_ok());
        assert!(NodeName::new("node123").is_ok());
        assert!(NodeName::new("test-node").is_ok());
        assert!(NodeName::new("MyNode").is_ok());

        // Invalid names
        assert!(NodeName::new("").is_err());
        assert!(NodeName::new("123node").is_err()); // starts with number
        assert!(NodeName::new("-node").is_err()); // starts with hyphen
        assert!(NodeName::new("_node").is_err()); // starts with underscore
        assert!(NodeName::new("node name").is_err()); // contains space
        assert!(NodeName::new("node@name").is_err()); // contains invalid character
        assert!(NodeName::new("node.name").is_err()); // contains dot
    }

    #[test]
    fn test_language_from_str() {
        assert_eq!(Language::from_str("python").unwrap(), Language::Python);
        assert_eq!(Language::from_str("rust").unwrap(), Language::Rust);
        assert!(Language::from_str("javascript").is_err());
        assert!(Language::from_str("").is_err());
    }
}
