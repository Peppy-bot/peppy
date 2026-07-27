//! The one validator for a core-node name, and the type that carries the
//! result.
//!
//! Before this existed, peppy checked core-node names in three independent
//! places (the CLI's `--core-node` parser, the daemon's `--core-node-name`
//! resolution, and `PeppyConfig::validate`), each re-deriving the same rule and
//! each phrasing its refusal differently. Federation adds a fourth rule (`self`
//! is reserved) and two more call sites (launcher core node link ids and
//! `--place` targets), so three copies would have become six.
//!
//! Parse, don't validate: everything downstream takes a [`CoreNodeName`], so
//! there is no way to reach a call site holding a name nobody checked. That is
//! what makes "`self` is rejected everywhere peppy validates a core node name"
//! a property of the type rather than a rule six places have to remember.

use crate::peppy_config::MAX_CORE_NODE_NAME_LEN;
use config::consts::ALLOWED_CONFIG_CHARS;
use config::runtime::Name;
use serde::{Deserialize, Deserializer, Serialize};

/// The reserved name meaning "the daemon this command is aimed at".
///
/// `self` is spelled out at the authoring surface (`--place foo@self`, and the
/// launcher's `core_node` field resolves through it) and never reaches the
/// wire: the CLI substitutes the coordinator's real name before dispatch, so a
/// daemon only ever sees concrete names. Reserving it means a real machine can
/// never be called `self`, which would otherwise make `--place foo@self`
/// ambiguous with no way for the reader to tell which was meant.
pub const SELF_CORE_NODE: &str = "self";

/// A validated core-node name.
///
/// Construction is the only way to get one, so holding a `CoreNodeName` is
/// proof that the charset, the length cap, and the `self` reservation were all
/// checked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CoreNodeName(String);

/// Why a candidate core-node name was refused. Separate variants (rather than
/// one string) so every call site renders the same explanation and a test can
/// assert the reason rather than a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreNodeNameError {
    /// The reserved word. Named separately from the charset failure because
    /// `self` IS a valid `Name`, so the generic message would be baffling.
    Reserved,
    /// Empty, over the length cap, or containing a character outside
    /// [`ALLOWED_CONFIG_CHARS`].
    Malformed,
}

impl std::fmt::Display for CoreNodeNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reserved => write!(
                f,
                "`{SELF_CORE_NODE}` is reserved: it means \"the daemon this command targets\", \
                 so no core node may be named it. Rename the daemon (its \
                 `core_node_name`, or `peppy service serve --core-node-name`) and restart; \
                 it re-registers under the new name on its own."
            ),
            Self::Malformed => write!(
                f,
                "must be non-empty, at most {MAX_CORE_NODE_NAME_LEN} characters, and use only \
                 characters from \"{ALLOWED_CONFIG_CHARS}\""
            ),
        }
    }
}

impl std::error::Error for CoreNodeNameError {}

impl CoreNodeName {
    /// The single validator. Every other core-node-name check in peppy calls
    /// this; a test pins that there is exactly one.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreNodeNameError> {
        let value = value.into();
        if value == SELF_CORE_NODE {
            return Err(CoreNodeNameError::Reserved);
        }
        if Name::new(value.as_str()).is_err() || value.len() > MAX_CORE_NODE_NAME_LEN {
            return Err(CoreNodeNameError::Malformed);
        }
        Ok(Self(value))
    }

    /// Whether `value` is the reserved `self` keyword. For the authoring
    /// surfaces that must accept it and resolve it before validation.
    pub fn is_self_keyword(value: &str) -> bool {
        value == SELF_CORE_NODE
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for CoreNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CoreNodeName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CoreNodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_an_ordinary_name() {
        let name = CoreNodeName::new("cn-atlas-h100").expect("valid");
        assert_eq!(name.as_str(), "cn-atlas-h100");
    }

    #[test]
    fn rejects_the_reserved_self_keyword() {
        assert_eq!(
            CoreNodeName::new(SELF_CORE_NODE),
            Err(CoreNodeNameError::Reserved)
        );
    }

    /// `self` is a perfectly good `Name`, so without its own variant the
    /// refusal would read as a charset complaint about a name whose charset is
    /// fine.
    #[test]
    fn the_reserved_refusal_explains_itself_and_names_the_fix() {
        let message = CoreNodeNameError::Reserved.to_string();
        assert!(message.contains("reserved"), "got: {message}");
        assert!(message.contains("core_node_name"), "got: {message}");
        assert!(message.contains("restart"), "got: {message}");
    }

    #[test]
    fn rejects_empty_and_overlong_and_bad_charset() {
        for candidate in [
            String::new(),
            "n".repeat(MAX_CORE_NODE_NAME_LEN + 1),
            "has space".to_owned(),
            "has/slash".to_owned(),
        ] {
            assert_eq!(
                CoreNodeName::new(candidate.clone()),
                Err(CoreNodeNameError::Malformed),
                "{candidate:?} must be refused"
            );
        }
    }

    #[test]
    fn accepts_a_name_at_exactly_the_length_cap() {
        let at_cap = "n".repeat(MAX_CORE_NODE_NAME_LEN);
        assert!(CoreNodeName::new(at_cap).is_ok());
    }

    #[test]
    fn is_self_keyword_recognizes_only_the_exact_word() {
        assert!(CoreNodeName::is_self_keyword("self"));
        assert!(!CoreNodeName::is_self_keyword("selfish"));
        assert!(!CoreNodeName::is_self_keyword("Self"));
    }

    /// Deserialization runs the same validator, so a hand-edited config or a
    /// launcher document cannot smuggle an unchecked name past the type.
    #[test]
    fn deserialization_applies_the_same_rules() {
        let name: CoreNodeName = serde_json5::from_str(r#""cn-robot-7""#).expect("valid");
        assert_eq!(name.as_str(), "cn-robot-7");

        let error = serde_json5::from_str::<CoreNodeName>(r#""self""#)
            .expect_err("`self` must not deserialize");
        assert!(error.to_string().contains("reserved"), "got: {error}");
    }

    #[test]
    fn round_trips_through_serde() {
        let name = CoreNodeName::new("cn-robot-7").expect("valid");
        let encoded = serde_json5::to_string(&name).expect("serialize");
        assert_eq!(encoded, r#""cn-robot-7""#);
        assert_eq!(
            serde_json5::from_str::<CoreNodeName>(&encoded).expect("deserialize"),
            name
        );
    }
}
