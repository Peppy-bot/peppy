//! Marks and enums of message formats and contracts.
//!
//! A mark is a name that a contract gives to a field of a message format
//! (`$marks` on the field) or to a member (`marks` on the member). Peppy
//! defines one mark and acts on it, [`MOVES_ROBOT`]. The parser keeps every
//! other mark as written and gives it to its clients: each client defines
//! the marks that it reads. An enum (`$enum` on a `string` field) is a
//! closed set of strings that a field holds. Peppy carries the enums and
//! acts on none: the wire holds the text of the field, and the checkers of
//! the clients read the set.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// The mark of a member that can move a part of a robot, the gripper
/// included, or change the state of a device. Each MCP server, `peppy node
/// call` and each lease holder asks the daemon whether it may move the robot
/// before each call of a member with this mark.
pub const MOVES_ROBOT: &str = "moves_robot";

/// The name of a mark: 1 to [`MarkName::MAX_LEN`] bytes, made of words of
/// lowercase ASCII letters and digits that each start with a letter, joined
/// by one `_`, such as `moves_robot` or `item_id`: the rule of the names of
/// failure kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MarkName(String);

/// Why a text is not a [`MarkName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid mark name {name:?}: {reason}")]
pub struct InvalidMarkName {
    pub name: String,
    pub reason: &'static str,
}

impl MarkName {
    /// The longest name, in bytes.
    pub const MAX_LEN: usize = crate::internal::word_name::MAX_WORD_NAME_LEN;

    pub fn parse(name: &str) -> Result<Self, InvalidMarkName> {
        match crate::internal::word_name::word_name_problem(name) {
            Some(reason) => Err(InvalidMarkName {
                name: name.to_owned(),
                reason,
            }),
            None => Ok(Self(name.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MarkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MarkName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).map_err(serde::de::Error::custom)
    }
}

/// The marks of a field or a member, each once, in the order they are
/// written. Empty when the document gives none; a written list names at
/// least one mark.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Marks(Vec<MarkName>);

/// Why a written list of marks is not [`Marks`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMarks {
    #[error("the list of marks is empty; leave it out for a field or a member with no mark")]
    Empty,
    #[error("mark `{0}` is in the list twice")]
    Repeated(String),
}

impl Marks {
    /// No mark: the marks of a field or a member that the document gives
    /// none.
    pub const fn none() -> Self {
        Self(Vec::new())
    }

    /// The marks `names`, as a document writes them: at least one, each
    /// once.
    pub fn new(names: Vec<MarkName>) -> Result<Self, InvalidMarks> {
        if names.is_empty() {
            return Err(InvalidMarks::Empty);
        }
        for (index, name) in names.iter().enumerate() {
            if names[..index].contains(name) {
                return Err(InvalidMarks::Repeated(name.to_string()));
            }
        }
        Ok(Self(names))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &MarkName> {
        self.0.iter()
    }

    /// Whether the list holds the mark `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.0.iter().any(|mark| mark.as_str() == name)
    }

    /// Whether the list holds [`MOVES_ROBOT`].
    pub fn moves_robot(&self) -> bool {
        self.contains(MOVES_ROBOT)
    }
}

impl<'de> Deserialize<'de> for Marks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<MarkName>::deserialize(deserializer)?;
        Self::new(names).map_err(serde::de::Error::custom)
    }
}

/// The closed set of strings that a `string` field holds: at least one
/// value, each a non-empty string, each once, in the order they are written.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct EnumValues(Vec<String>);

/// Why a written `$enum` is not [`EnumValues`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidEnumValues {
    #[error("`$enum` lists no value; an enum holds at least one value")]
    Empty,
    #[error("`$enum` lists an empty string; each value is a non-empty string")]
    EmptyValue,
    #[error("`$enum` lists value {0:?} twice")]
    Repeated(String),
}

impl EnumValues {
    pub fn new(values: Vec<String>) -> Result<Self, InvalidEnumValues> {
        if values.is_empty() {
            return Err(InvalidEnumValues::Empty);
        }
        for (index, value) in values.iter().enumerate() {
            if value.is_empty() {
                return Err(InvalidEnumValues::EmptyValue);
            }
            if values[..index].contains(value) {
                return Err(InvalidEnumValues::Repeated(value.clone()));
            }
        }
        Ok(Self(values))
    }

    pub fn values(&self) -> &[String] {
        &self.0
    }

    /// Whether `value` is one of the values of the set.
    pub fn contains(&self, value: &str) -> bool {
        self.0.iter().any(|held| held == value)
    }
}

impl<'de> Deserialize<'de> for EnumValues {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = Vec::<String>::deserialize(deserializer)?;
        Self::new(values).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_name_is_words_of_lowercase_letters_and_digits() {
        for name in ["moves_robot", "item_id", "x", "pose2d"] {
            assert!(MarkName::parse(name).is_ok(), "{name}");
        }
        for name in [
            "",
            "Moves",
            "moves__robot",
            "_moves",
            "moves_",
            "2d",
            "a-b",
            "a_2",
        ] {
            assert!(MarkName::parse(name).is_err(), "{name}");
        }
        assert!(MarkName::parse(&"a".repeat(65)).is_err());
        assert!(MarkName::parse(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn a_written_list_of_marks_names_each_mark_once() {
        let parse = |text: &str| serde_json::from_str::<Marks>(text);
        let marks = parse(r#"["moves_robot", "item_id"]"#).unwrap();
        assert!(marks.moves_robot());
        assert!(marks.contains("item_id"));
        assert!(!marks.contains("location"));
        assert!(
            parse("[]")
                .unwrap_err()
                .to_string()
                .contains("leave it out")
        );
        assert!(
            parse(r#"["item_id", "item_id"]"#)
                .unwrap_err()
                .to_string()
                .contains("`item_id` is in the list twice")
        );
        assert!(parse(r#"["Item"]"#).is_err());
    }

    #[test]
    fn an_enum_holds_distinct_non_empty_values() {
        let parse = |text: &str| serde_json::from_str::<EnumValues>(text);
        let values = parse(r#"["join", "remove"]"#).unwrap();
        assert_eq!(values.values(), ["join", "remove"]);
        assert!(values.contains("join"));
        assert!(!values.contains("reset"));
        assert_eq!(
            parse("[]").unwrap_err().to_string(),
            InvalidEnumValues::Empty.to_string()
        );
        assert!(parse(r#"["a", ""]"#).is_err());
        assert!(
            parse(r#"["a", "a"]"#)
                .unwrap_err()
                .to_string()
                .contains("twice")
        );
    }
}
