//! Failure kinds: the cause of a refused or failed goal, or of the error
//! reply of a service.
//!
//! A member gives a kind from a closed set: the kinds that peppy holds
//! ([`PeppyFailureKind`]), which every provider can give, and the kinds of
//! its contract that the member lists. A contract declares its kinds, each
//! with its meaning, in its `failure_kinds` block ([`ContractFailureKinds`]),
//! and each of its services and actions lists in a `failure_kinds` list of
//! its own the kinds of that block it gives ([`ContractKinds`]). A member
//! that lists none, or that no contract declares, gives the kinds of peppy
//! only. [`MemberFailureKinds`] is the closed set of one member.

use crate::internal::node::{
    ActionTopicEndpoint, GoalServiceEndpoint, MessageFormat, NativeExposedAction,
    NativeExposedService, ResultServiceEndpoint,
};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The name of a failure kind: 1 to [`FailureKindName::MAX_LEN`] bytes, made
/// of words of lowercase ASCII letters and digits that each start with a
/// letter, joined by one `_`, such as `unreachable` or `leader_streaming`.
/// So two names never give the same identifier in another case: `kind_2`,
/// `kind__lost` and `kind_` are not names.
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
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(invalid(
                "it may hold only lowercase ASCII letters, digits and `_`",
            ));
        }
        if !name
            .split('_')
            .all(|word| word.starts_with(|c: char| c.is_ascii_lowercase()))
        {
            return Err(invalid(
                "it must be words that each start with a lowercase ASCII letter, joined by one `_`",
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

/// Which section of a contract a member sits in. A service and an action
/// of one contract can share a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberSection {
    Service,
    Action,
}

impl fmt::Display for MemberSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Service => "service",
            Self::Action => "action",
        })
    }
}

/// The failure kinds of a contract: its `failure_kinds` block, and the kinds
/// of the block that each of its services and actions lists. Each kind that
/// a member lists is declared, and each declared kind is listed by a member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractKinds {
    declared: ContractFailureKinds,
    members: BTreeMap<(MemberSection, String), ContractFailureKinds>,
}

/// Why the `failure_kinds` lists of the members of a contract do not fit
/// its `failure_kinds` block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMemberKinds {
    #[error(
        "{section} `{member}` lists failure kind `{kind}`, which the `failure_kinds` block of the contract does not declare: declare it there, or remove it from the list; a kind of peppy is never listed"
    )]
    Undeclared {
        section: MemberSection,
        member: String,
        kind: String,
    },
    #[error("{section} `{member}` lists failure kind `{kind}` twice")]
    Repeated {
        section: MemberSection,
        member: String,
        kind: String,
    },
    #[error(
        "failure kind `{0}` of the `failure_kinds` block is listed by no service or action: list it on each member that gives it, or remove it"
    )]
    Unlisted(String),
}

impl ContractKinds {
    /// The kinds of a contract that declares `declared` and whose members
    /// list `members`: each its section, its name and the kinds it lists.
    pub fn new(
        declared: ContractFailureKinds,
        members: impl IntoIterator<Item = (MemberSection, String, Vec<FailureKindName>)>,
    ) -> Result<Self, InvalidMemberKinds> {
        let mut listed = BTreeMap::new();
        for (section, member, kinds) in members {
            let mut given: IndexMap<FailureKindName, String> = IndexMap::new();
            for kind in kinds {
                let Some((_, meaning)) = declared.iter().find(|(name, _)| **name == kind) else {
                    return Err(InvalidMemberKinds::Undeclared {
                        section,
                        member,
                        kind: kind.to_string(),
                    });
                };
                let meaning = meaning.to_owned();
                if given.insert(kind.clone(), meaning).is_some() {
                    return Err(InvalidMemberKinds::Repeated {
                        section,
                        member,
                        kind: kind.to_string(),
                    });
                }
            }
            if !given.is_empty() {
                listed.insert((section, member), in_declared_order(&declared, given));
            }
        }
        if let Some((unlisted, _)) = declared.iter().find(|(name, _)| {
            !listed
                .values()
                .any(|kinds: &ContractFailureKinds| kinds.contains(name.as_str()))
        }) {
            return Err(InvalidMemberKinds::Unlisted(unlisted.to_string()));
        }
        Ok(Self {
            declared,
            members: listed,
        })
    }

    /// The `failure_kinds` block of the contract.
    pub fn declared(&self) -> &ContractFailureKinds {
        &self.declared
    }

    /// The kinds that the member `name` of `section` gives: the kinds of
    /// peppy, and the kinds of the contract that it lists.
    pub fn of(&self, section: MemberSection, name: &str) -> MemberFailureKinds {
        self.members
            .get(&(section, name.to_owned()))
            .cloned()
            .map(MemberFailureKinds::of_member)
            .unwrap_or_default()
    }

    /// The names of the kinds of the contract that the member `name` of
    /// `section` lists, in the order of the block.
    pub fn listed(&self, section: MemberSection, name: &str) -> Vec<FailureKindName> {
        self.members
            .get(&(section, name.to_owned()))
            .map(|kinds| kinds.iter().map(|(kind, _)| kind.clone()).collect())
            .unwrap_or_default()
    }
}

/// A service of a contract document: the shape of a native service, and
/// the kinds of the contract that it gives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractService {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_kinds: Vec<FailureKindName>,
}

/// An action of a contract document: the shape of a native action, and the
/// kinds of the contract that it gives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractAction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_service: Option<GoalServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_topic: Option<ActionTopicEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_service: Option<ResultServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_kinds: Vec<FailureKindName>,
}

/// The services and actions of a contract, each with the shape of a native
/// member, and the kinds of the contract that each gives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContractMembers {
    pub services: Vec<NativeExposedService>,
    pub actions: Vec<NativeExposedAction>,
    pub failure_kinds: ContractKinds,
}

impl ContractMembers {
    /// The members of a contract document that declares `declared` in its
    /// `failure_kinds` block and lists `services` and `actions`.
    pub fn new(
        declared: ContractFailureKinds,
        services: Vec<ContractService>,
        actions: Vec<ContractAction>,
    ) -> Result<Self, InvalidMemberKinds> {
        let mut listed = Vec::with_capacity(services.len() + actions.len());
        let services = services
            .into_iter()
            .map(|service| {
                let ContractService {
                    name,
                    request_message_format,
                    response_message_format,
                    failure_kinds,
                } = service;
                listed.push((MemberSection::Service, name.clone(), failure_kinds));
                NativeExposedService {
                    name,
                    request_message_format,
                    response_message_format,
                }
            })
            .collect();
        let actions = actions
            .into_iter()
            .map(|action| {
                let ContractAction {
                    name,
                    goal_service,
                    feedback_topic,
                    result_service,
                    failure_kinds,
                } = action;
                listed.push((MemberSection::Action, name.clone(), failure_kinds));
                NativeExposedAction {
                    name,
                    goal_service,
                    feedback_topic,
                    result_service,
                }
            })
            .collect();
        Ok(Self {
            services,
            actions,
            failure_kinds: ContractKinds::new(declared, listed)?,
        })
    }

    /// The services of the document, each with the kinds it lists.
    pub fn documented_services(&self) -> Vec<ContractService> {
        self.services
            .iter()
            .map(|service| ContractService {
                name: service.name.clone(),
                request_message_format: service.request_message_format.clone(),
                response_message_format: service.response_message_format.clone(),
                failure_kinds: self
                    .failure_kinds
                    .listed(MemberSection::Service, &service.name),
            })
            .collect()
    }

    /// The actions of the document, each with the kinds it lists.
    pub fn documented_actions(&self) -> Vec<ContractAction> {
        self.actions
            .iter()
            .map(|action| ContractAction {
                name: action.name.clone(),
                goal_service: action.goal_service.clone(),
                feedback_topic: action.feedback_topic.clone(),
                result_service: action.result_service.clone(),
                failure_kinds: self
                    .failure_kinds
                    .listed(MemberSection::Action, &action.name),
            })
            .collect()
    }
}

/// `given`, a subset of `declared`, in the order of `declared`.
fn in_declared_order(
    declared: &ContractFailureKinds,
    given: IndexMap<FailureKindName, String>,
) -> ContractFailureKinds {
    ContractFailureKinds(
        declared
            .iter()
            .filter(|(name, _)| given.contains_key(*name))
            .map(|(name, meaning)| (name.clone(), meaning.to_owned()))
            .collect(),
    )
}

/// The closed set of failure kinds that one member can give: the kinds of
/// peppy and the kinds of its contract that the member lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberFailureKinds {
    contract: ContractFailureKinds,
}

impl MemberFailureKinds {
    /// The kinds of a member that gives `contract`, kinds of its contract,
    /// beside the kinds of peppy.
    pub fn of_member(contract: ContractFailureKinds) -> Self {
        Self { contract }
    }

    /// The kinds of a member that gives no kind of a contract: the kinds of
    /// peppy.
    pub fn peppy_only() -> Self {
        Self::default()
    }

    /// The kinds of its contract that the member gives.
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
    fn a_name_is_words_that_start_with_a_letter_joined_by_one_underscore() {
        for valid in ["unreachable", "leader_streaming", "a", "kind2", "v2_kind"] {
            assert_eq!(name(valid).as_str(), valid);
        }
        for invalid in [
            "",
            "Unreachable",
            "2kind",
            "_kind",
            "kind_",
            "kind__lost",
            "kind_2",
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
        let member = MemberFailureKinds::of_member(contract);
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
        let member = MemberFailureKinds::of_member(contract);
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

    /// The block of a contract with `unreachable` and `leader_streaming`.
    fn motion_block() -> ContractFailureKinds {
        ContractFailureKinds::new([
            (name("unreachable"), "No plan to the target".to_owned()),
            (
                name("leader_streaming"),
                "A leader streams the limb".to_owned(),
            ),
        ])
        .unwrap()
    }

    fn listing(
        section: MemberSection,
        member: &str,
        kinds: &[&str],
    ) -> (MemberSection, String, Vec<FailureKindName>) {
        (
            section,
            member.to_owned(),
            kinds.iter().map(|kind| name(kind)).collect(),
        )
    }

    /// Each member gives the kinds of peppy and the kinds it lists, in the
    /// order of the block; a member that lists none gives the kinds of peppy
    /// only, and a service and an action of one name are two members.
    #[test]
    fn each_member_gives_the_kinds_it_lists_in_the_order_of_the_block() {
        let kinds = ContractKinds::new(
            motion_block(),
            [
                listing(
                    MemberSection::Action,
                    "move_arm",
                    &["leader_streaming", "unreachable"],
                ),
                listing(MemberSection::Action, "move_gripper", &["leader_streaming"]),
                listing(MemberSection::Service, "move_gripper", &[]),
            ],
        )
        .unwrap();

        let move_arm = kinds.of(MemberSection::Action, "move_arm");
        let contract_kinds: Vec<&str> = move_arm
            .contract_kinds()
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect();
        assert_eq!(contract_kinds, ["unreachable", "leader_streaming"]);
        assert_eq!(
            kinds.listed(MemberSection::Action, "move_arm"),
            [name("unreachable"), name("leader_streaming")]
        );

        let gripper = kinds.of(MemberSection::Action, "move_gripper");
        assert!(gripper.allows("leader_streaming"));
        assert!(!gripper.allows("unreachable"));
        assert!(gripper.allows("rejected"));

        assert_eq!(
            kinds.of(MemberSection::Service, "move_gripper"),
            MemberFailureKinds::peppy_only()
        );
        assert_eq!(
            kinds.of(MemberSection::Action, "stop"),
            MemberFailureKinds::peppy_only()
        );
    }

    #[test]
    fn a_member_cannot_list_a_kind_that_the_block_does_not_declare() {
        for undeclared in ["not_found", "timeout"] {
            let err = ContractKinds::new(
                motion_block(),
                [
                    listing(
                        MemberSection::Action,
                        "move_arm",
                        &["unreachable", "leader_streaming"],
                    ),
                    listing(MemberSection::Service, "check_arm_move", &[undeclared]),
                ],
            )
            .unwrap_err();
            assert_eq!(
                err,
                InvalidMemberKinds::Undeclared {
                    section: MemberSection::Service,
                    member: "check_arm_move".to_owned(),
                    kind: undeclared.to_owned(),
                }
            );
        }
    }

    #[test]
    fn a_member_lists_a_kind_once() {
        let err = ContractKinds::new(
            motion_block(),
            [listing(
                MemberSection::Action,
                "move_arm",
                &["unreachable", "leader_streaming", "unreachable"],
            )],
        )
        .unwrap_err();
        assert_eq!(
            err,
            InvalidMemberKinds::Repeated {
                section: MemberSection::Action,
                member: "move_arm".to_owned(),
                kind: "unreachable".to_owned(),
            }
        );
    }

    #[test]
    fn each_declared_kind_is_listed_by_a_member() {
        let err = ContractKinds::new(
            motion_block(),
            [listing(MemberSection::Action, "move_arm", &["unreachable"])],
        )
        .unwrap_err();
        assert_eq!(
            err,
            InvalidMemberKinds::Unlisted("leader_streaming".to_owned())
        );
    }
}
