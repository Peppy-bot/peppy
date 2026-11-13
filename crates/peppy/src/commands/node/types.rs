use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// A validated node name that ensures it follows naming conventions.
/// Node names must start with a letter and contain only alphanumeric characters,
/// underscores, or hyphens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeName(String);

impl NodeName {
    /// Creates a new `NodeName` after validating the input.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();

        let Some(first) = name.chars().next() else {
            return Err(Error::InvalidNodeName(
                "Node name cannot be empty".to_string(),
            ));
        };

        if !first.is_ascii_alphabetic() {
            return Err(Error::InvalidNodeName(format!(
                "Node name '{name}' must start with a letter"
            )));
        }

        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::InvalidNodeName(format!(
                "Node name '{name}' contains invalid characters"
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
        f.write_str(&self.0)
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
        NodeName::new("my_node").unwrap();
        NodeName::new("node123").unwrap();
        NodeName::new("test-node").unwrap();
        NodeName::new("MyNode").unwrap();

        // Invalid names
        NodeName::new("").unwrap_err();
        NodeName::new("123node").unwrap_err(); // starts with number
        NodeName::new("-node").unwrap_err(); // starts with hyphen
        NodeName::new("_node").unwrap_err(); // starts with underscore
        NodeName::new("node name").unwrap_err(); // contains space
        NodeName::new("node@name").unwrap_err(); // contains invalid character
        NodeName::new("node.name").unwrap_err(); // contains dot
    }

    #[test]
    fn node_name_refusals_name_the_broken_rule() {
        let refusal = |name: &str| {
            NodeName::new(name)
                .expect_err("the name should be refused")
                .to_string()
        };

        assert_eq!(refusal(""), "Invalid node name: Node name cannot be empty");
        assert_eq!(
            refusal("1node"),
            "Invalid node name: Node name '1node' must start with a letter"
        );
        assert_eq!(
            refusal("node name"),
            "Invalid node name: Node name 'node name' contains invalid characters"
        );
    }

    #[test]
    fn node_name_displays_the_name_as_given() {
        let name = NodeName::new("My-node_2").expect("valid node name");

        assert_eq!(name.to_string(), "My-node_2");
    }
}
