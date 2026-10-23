//! Failure kinds: the cause of a refused or failed goal, or of the error
//! reply of a service.
//!
//! A member gives a kind from a closed set: the kinds that peppy holds
//! ([`PeppyFailureKind`]), which every provider can give, and the kinds that
//! the contract of the member declares in its `failure_kinds` block
//! ([`ContractFailureKinds`]). A member that no contract declares gives the
//! kinds of peppy only. [`MemberFailureKinds`] is that closed set.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// The name of a failure kind: 1 to [`FailureKindName::MAX_LEN`] bytes of
/// lowercase ASCII letters, digits and `_`, that starts with a letter, such
/// as `unreachable` or `leader_streaming`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct FailureKindName(String);

/// Why a text is not a [`FailureKindName`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid failure kind name {name:?}: {reason}")]
pub struct InvalidFailureKindName {
    pub name: String,
    pub reason: &'static str,
}

impl FailureKindName {
    /// The longest name, in bytes. A name rides on the wire behind a one-byte
    /// length.
    pub const MAX_LEN: usize = 64;

    pub fn parse(name: &str) -> Result<Self, InvalidFailureKindName> {
        let invalid = |reason| InvalidFailureKindName {
            name: name.to_owned(),
            reason,
        };
        if name.is_empty() {
            return Err(invalid("it is empty"));
        }
        if name.len() > Self::MAX_LEN {
            return Err(invalid("it is longer than 64 bytes"));
        }
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err(invalid("it must start with a lowercase ASCII letter"));
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(invalid(
                "it may hold only lowercase ASCII letters, digits and `_`",
            ));
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<PeppyFailureKind> for FailureKindName {
    fn from(kind: PeppyFailureKind) -> Self {
        Self(kind.name().to_owned())
    }
}

impl fmt::Display for FailureKindName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FailureKindName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).map_err(serde::de::Error::custom)
    }
}

/// The failure kinds that peppy holds. Every provider can give them, and no
/// contract declares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeppyFailureKind {
    AttemptFailed,
    Timeout,
    PreconditionFailed,
    ResourceUnavailable,
    Rejected,
}

impl PeppyFailureKind {
    /// Every kind of peppy, in the order the documentation lists them.
    pub const ALL: [Self; 5] = [
        Self::AttemptFailed,
        Self::Timeout,
        Self::PreconditionFailed,
        Self::ResourceUnavailable,
        Self::Rejected,
    ];

    /// The name of the kind on the wire and in documents.
    pub const fn name(self) -> &'static str {
        match self {
            Self::AttemptFailed => "attempt_failed",
            Self::Timeout => "timeout",
            Self::PreconditionFailed => "precondition_failed",
            Self::ResourceUnavailable => "resource_unavailable",
            Self::Rejected => "rejected",
        }
    }

    /// What the kind means.
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::AttemptFailed => "The attempt gave no result",
            Self::Timeout => "The bound of the provider passed",
            Self::PreconditionFailed => "A precondition failed",
            Self::ResourceUnavailable => "A robot, limb or host is gone",
            Self::Rejected => "The provider refused the goal, and no other kind says why",
        }
    }

    /// The kind of peppy that `name` names, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

impl fmt::Display for PeppyFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The `failure_kinds` block of a contract: each kind of its own, in the
/// order the contract declares them, with its meaning. No name is a kind of
/// peppy, and no meaning is blank.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ContractFailureKinds(IndexMap<FailureKindName, String>);

/// Why a `failure_kinds` block is not valid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidContractFailureKinds {
    #[error(
        "failure kind `{0}` is a kind of peppy, which every provider can give: a contract does not declare it"
    )]
    PeppyKind(String),
    #[error("failure kind `{0}` has no meaning: give it a short sentence that says what it means")]
    BlankMeaning(String),
}

impl ContractFailureKinds {
    pub fn new(
        kinds: impl IntoIterator<Item = (FailureKindName, String)>,
    ) -> Result<Self, InvalidContractFailureKinds> {
        let kinds: IndexMap<FailureKindName, String> = kinds.into_iter().collect();
        for (name, meaning) in &kinds {
            if PeppyFailureKind::from_name(name.as_str()).is_some() {
                return Err(InvalidContractFailureKinds::PeppyKind(name.to_string()));
            }
            if meaning.trim().is_empty() {
                return Err(InvalidContractFailureKinds::BlankMeaning(name.to_string()));
            }
        }
        Ok(Self(kinds))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.keys().any(|kind| kind.as_str() == name)
    }

    /// Each kind and its meaning, in the order the contract declares them.
    pub fn iter(&self) -> impl Iterator<Item = (&FailureKindName, &str)> {
        self.0
            .iter()
            .map(|(name, meaning)| (name, meaning.as_str()))
    }
}

impl<'de> Deserialize<'de> for ContractFailureKinds {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let kinds = IndexMap::<FailureKindName, String>::deserialize(deserializer)?;
        Self::new(kinds).map_err(serde::de::Error::custom)
    }
}

/// The closed set of failure kinds that one member can give: the kinds of
/// peppy and the kinds of the member's contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberFailureKinds {
    contract: ContractFailureKinds,
}

impl MemberFailureKinds {
    /// The kinds of a member of a contract that declares `contract`.
    pub fn of_contract(contract: ContractFailureKinds) -> Self {
        Self { contract }
    }

    /// The kinds of a member that no contract declares: the kinds of peppy.
    pub fn peppy_only() -> Self {
        Self::default()
    }

    /// The kinds that the member's contract declares.
    pub fn contract_kinds(&self) -> &ContractFailureKinds {
        &self.contract
    }

    /// Whether the member can give the kind `name`.
    pub fn allows(&self, name: &str) -> bool {
        PeppyFailureKind::from_name(name).is_some() || self.contract.contains(name)
    }

    /// Each kind the member can give with its meaning: the kinds of peppy
    /// first, then the kinds of the contract.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        PeppyFailureKind::ALL
            .into_iter()
            .map(|kind| (kind.name(), kind.meaning()))
            .chain(
                self.contract
                    .iter()
                    .map(|(name, meaning)| (name.as_str(), meaning)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> FailureKindName {
        FailureKindName::parse(text).unwrap()
    }

    #[test]
    fn a_name_is_lowercase_snake_case_that_starts_with_a_letter() {
        for valid in ["unreachable", "leader_streaming", "a", "kind_2"] {
            assert_eq!(name(valid).as_str(), valid);
        }
        for invalid in [
            "",
            "Unreachable",
            "2kind",
            "_kind",
            "lost-item",
            "item lost",
        ] {
            assert!(
                FailureKindName::parse(invalid).is_err(),
                "{invalid:?} must not parse"
            );
        }
    }

    #[test]
    fn a_name_holds_at_most_64_bytes() {
        assert!(FailureKindName::parse(&"k".repeat(64)).is_ok());
        assert!(FailureKindName::parse(&"k".repeat(65)).is_err());
    }

    #[test]
    fn each_kind_of_peppy_has_a_valid_name_that_maps_back_to_it() {
        for kind in PeppyFailureKind::ALL {
            assert_eq!(name(kind.name()).as_str(), kind.name());
            assert_eq!(PeppyFailureKind::from_name(kind.name()), Some(kind));
            assert!(!kind.meaning().is_empty());
        }
        assert_eq!(PeppyFailureKind::from_name("unreachable"), None);
    }

    #[test]
    fn a_contract_block_keeps_the_declared_order() {
        let kinds: ContractFailureKinds =
            serde_json5::from_str(r#"{ zeta: "Last letter", alpha: "First letter" }"#).unwrap();
        let names: Vec<&str> = kinds.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[test]
    fn a_contract_block_refuses_a_kind_of_peppy() {
        let err = serde_json5::from_str::<ContractFailureKinds>(r#"{ timeout: "Too slow" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`timeout` is a kind of peppy"), "{err}");
    }

    #[test]
    fn a_contract_block_refuses_a_blank_meaning() {
        let err = serde_json5::from_str::<ContractFailureKinds>(r#"{ unreachable: "  " }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`unreachable` has no meaning"), "{err}");
    }

    #[test]
    fn a_contract_block_refuses_an_invalid_name() {
        let err = serde_json5::from_str::<ContractFailureKinds>(r#"{ "Lost-Item": "Gone" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid failure kind name"), "{err}");
    }

    #[test]
    fn a_member_allows_the_kinds_of_peppy_and_of_its_contract_only() {
        let contract =
            ContractFailureKinds::new([(name("unreachable"), "No plan to the target".to_owned())])
                .unwrap();
        let member = MemberFailureKinds::of_contract(contract);
        assert!(member.allows("unreachable"));
        assert!(member.allows("rejected"));
        assert!(!member.allows("leader_streaming"));

        let native = MemberFailureKinds::peppy_only();
        assert!(native.allows("attempt_failed"));
        assert!(!native.allows("unreachable"));
    }

    #[test]
    fn a_member_lists_the_kinds_of_peppy_before_the_kinds_of_its_contract() {
        let contract =
            ContractFailureKinds::new([(name("not_found"), "Nothing matches".to_owned())]).unwrap();
        let member = MemberFailureKinds::of_contract(contract);
        let names: Vec<&str> = member.iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            [
                "attempt_failed",
                "timeout",
                "precondition_failed",
                "resource_unavailable",
                "rejected",
                "not_found"
            ]
        );
    }
}
