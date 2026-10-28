//! `robot_leases:v1`: the fleet listing and its changes, the leases that give
//! one client the right to move one robot, the move check and the halt of the
//! stack. Its scope names the members that a client may call.
//!
//! The types below are the messages of the interface in their canonical
//! JSON form, as the run-time codec gives them and takes them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The interface's document, compiled in.
pub(super) const DOCUMENT: &str = include_str!("robot_leases.v1.json5");

/// A member of `robot_leases:v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RobotLeasesMember {
    List,
    Contract,
    Take,
    Release,
    Offer,
    Open,
    Close,
    CheckMove,
    Halt,
    ClearHalt,
    Changes,
    Lease,
}

impl RobotLeasesMember {
    /// Every member of the interface: its services, then its actions.
    pub const ALL: [Self; 12] = [
        Self::List,
        Self::Contract,
        Self::Take,
        Self::Release,
        Self::Offer,
        Self::Open,
        Self::Close,
        Self::CheckMove,
        Self::Halt,
        Self::ClearHalt,
        Self::Changes,
        Self::Lease,
    ];

    /// The member's name in the interface's document.
    pub fn name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Contract => "contract",
            Self::Take => "take",
            Self::Release => "release",
            Self::Offer => "offer",
            Self::Open => "open",
            Self::Close => "close",
            Self::CheckMove => "check_move",
            Self::Halt => "halt",
            Self::ClearHalt => "clear_halt",
            Self::Changes => "changes",
            Self::Lease => "lease",
        }
    }

    /// The member the document names `name`, if any.
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|member| member.name() == name)
    }

    /// Whether the member is an action; every other member is a service.
    pub fn is_action(self) -> bool {
        matches!(self, Self::Changes | Self::Lease)
    }

    /// The services that a lease holder calls while its `lease` goal runs,
    /// which a scope gives with `lease`.
    pub fn is_lease_service(self) -> bool {
        matches!(
            self,
            Self::Take | Self::Release | Self::Offer | Self::Open | Self::Close
        )
    }
}

/// A member that a `robot_leases` scope can name: each member but the
/// services of a lease holder, which `lease` brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopedRobotLeasesMember {
    List,
    Contract,
    Changes,
    Lease,
    CheckMove,
    Halt,
    ClearHalt,
}

impl ScopedRobotLeasesMember {
    fn member(self) -> RobotLeasesMember {
        match self {
            Self::List => RobotLeasesMember::List,
            Self::Contract => RobotLeasesMember::Contract,
            Self::Changes => RobotLeasesMember::Changes,
            Self::Lease => RobotLeasesMember::Lease,
            Self::CheckMove => RobotLeasesMember::CheckMove,
            Self::Halt => RobotLeasesMember::Halt,
            Self::ClearHalt => RobotLeasesMember::ClearHalt,
        }
    }
}

/// The scope of a daemon target of `robot_leases:v1`: the members that its
/// client may call.
/// `lease` brings the services of a lease holder (`take`, `release`,
/// `offer`, `open` and `close`), and `check_move` answers every client with
/// or without a scope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawRobotLeasesScope")]
pub struct RobotLeasesScope {
    members: BTreeSet<ScopedRobotLeasesMember>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRobotLeasesScope {
    members: Vec<String>,
}

impl TryFrom<RawRobotLeasesScope> for RobotLeasesScope {
    type Error = String;

    fn try_from(raw: RawRobotLeasesScope) -> Result<Self, String> {
        if raw.members.is_empty() {
            return Err(
                "`members` names no member; a `robot_leases` scope names at least one member of \
                 `robot_leases:v1`"
                    .to_owned(),
            );
        }
        let mut members = BTreeSet::new();
        for name in &raw.members {
            let member = match RobotLeasesMember::named(name) {
                Some(member) if member.is_lease_service() => {
                    return Err(format!(
                        "`members` names `{name}`, which `lease` brings with the other services \
                         of a lease holder; name `lease`"
                    ));
                }
                Some(member) => member,
                None => {
                    return Err(format!(
                        "`members` names `{name}`, which is not a member of `robot_leases:v1`; a \
                         scope names list, contract, changes, lease, check_move, halt and \
                         clear_halt"
                    ));
                }
            };
            let scoped = ScopedRobotLeasesMember::ALL
                .into_iter()
                .find(|scoped| scoped.member() == member)
                .expect("each member that is not a lease service can be scoped");
            if !members.insert(scoped) {
                return Err(format!("`members` names `{name}` twice"));
            }
        }
        Ok(Self { members })
    }
}

impl ScopedRobotLeasesMember {
    const ALL: [Self; 7] = [
        Self::List,
        Self::Contract,
        Self::Changes,
        Self::Lease,
        Self::CheckMove,
        Self::Halt,
        Self::ClearHalt,
    ];
}

impl RobotLeasesScope {
    /// Leaves out of `bundle` each tool and task of the daemon target
    /// `target` whose member the scope does not give: the server serves no
    /// tool that the daemon refuses.
    pub(super) fn narrow(&self, bundle: &mut peppy_mcp_catalog::ExposureBundle, target: &str) {
        let given = |member: &str| {
            RobotLeasesMember::named(member).is_some_and(|member| self.allows(member))
        };
        bundle
            .tools
            .retain(|tool| tool.target != target || given(&tool.member));
        bundle
            .tasks
            .retain(|task| task.target != target || given(&task.member));
    }

    /// Whether the scope lets its client call `member`.
    pub fn allows(&self, member: RobotLeasesMember) -> bool {
        if member == RobotLeasesMember::CheckMove {
            return true;
        }
        if member.is_lease_service() {
            return self.members.contains(&ScopedRobotLeasesMember::Lease);
        }
        self.members.iter().any(|scoped| scoped.member() == member)
    }
}

/// The kind of a robot: simulated when every instance of its copy reads the
/// clock of a simulation, else physical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotKind {
    Simulated,
    Physical,
}

/// Whether every instance of a robot's copy runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftwareState {
    Running,
    Stopped,
}

/// The fleet listing: the answer of `list`, and the listing of each
/// feedback of `changes`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetListing {
    pub version: String,
    pub core_node: String,
    pub launcher: String,
    pub halted: bool,
    pub halted_by: String,
    pub robots: Vec<FleetRobot>,
    pub leases: Vec<FleetLease>,
    pub options: Vec<FleetOption>,
    pub documents: Vec<FleetDocument>,
    pub nodes: Vec<FleetNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRobot {
    pub name: String,
    pub option: String,
    pub core_node: String,
    pub kind: RobotKind,
    pub clock: String,
    pub software: SoftwareState,
    pub robot_commander: String,
    pub instances: Vec<FleetInstance>,
    pub contracts: Vec<RobotContract>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetInstance {
    pub name: String,
    /// The id that the stack runs the instance under.
    pub instance: String,
    pub core_node: String,
    pub node: String,
    pub node_tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotContract {
    pub name: String,
    pub tag: String,
    pub sha256: String,
    pub instances: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetLease {
    pub robot: String,
    pub holder: String,
    pub holder_core_node: String,
    pub name: String,
    pub open: bool,
    pub giver: String,
    pub giver_core_node: String,
    pub receiver: String,
    pub receiver_core_node: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetOption {
    pub option: String,
    pub kind: RobotKind,
    pub contracts: Vec<ContractPin>,
    pub features: Vec<FleetFeature>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ContractPin {
    pub name: String,
    pub tag: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetFeature {
    pub feature: String,
    pub contracts: Vec<ContractPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetDocument {
    pub name: String,
    pub tag: String,
    pub sha256: String,
    pub document: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNode {
    pub name: String,
    pub tag: String,
    pub machines: Vec<NodeMachine>,
    pub instances: Vec<NodeInstance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeMachine {
    pub core_node: String,
    pub manifest_sha256: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInstance {
    /// The id that the stack runs the instance under.
    pub instance: String,
    pub core_node: String,
    pub robot: String,
}

/// The request of `contract`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractRequest {
    pub name: String,
    pub tag: String,
    pub sha256: String,
}

/// The response of `contract`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractResponse {
    pub document: String,
}

/// The request of `take`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeRequest {
    pub robot: String,
    pub name: String,
}

/// The request of `release`, `open` and `close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotRequest {
    pub robot: String,
}

/// The request of `offer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferRequest {
    pub robots: Vec<String>,
    pub receiver: String,
}

/// The request of `check_move`: the instance that the client calls, or the
/// robot that it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckMoveRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub robot: Option<String>,
}

/// What a `check_move` request asks about, parsed: exactly one of the
/// instance and the robot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveTarget {
    Instance(String),
    Robot(String),
}

impl TryFrom<CheckMoveRequest> for MoveTarget {
    type Error = &'static str;

    fn try_from(request: CheckMoveRequest) -> Result<Self, Self::Error> {
        match (request.instance, request.robot) {
            (Some(instance), None) => Ok(Self::Instance(instance)),
            (None, Some(robot)) => Ok(Self::Robot(robot)),
            _ => Err("a check names exactly one of `instance` and `robot`"),
        }
    }
}

impl From<MoveTarget> for CheckMoveRequest {
    fn from(target: MoveTarget) -> Self {
        match target {
            MoveTarget::Instance(instance) => Self {
                instance: Some(instance),
                robot: None,
            },
            MoveTarget::Robot(robot) => Self {
                instance: None,
                robot: Some(robot),
            },
        }
    }
}

/// The failure kinds that `robot_leases:v1` declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotLeasesFailureKind {
    Halted,
    Leased,
    NotLeased,
}

impl RobotLeasesFailureKind {
    /// The name of the kind on the wire and in the document.
    pub fn name(self) -> &'static str {
        match self {
            Self::Halted => "halted",
            Self::Leased => "leased",
            Self::NotLeased => "not_leased",
        }
    }
}

impl From<RobotLeasesFailureKind> for config::failure_kind::FailureKindName {
    fn from(kind: RobotLeasesFailureKind) -> Self {
        Self::parse(kind.name()).expect("each kind of robot_leases:v1 is a valid failure kind name")
    }
}

/// A `u64` in the canonical JSON form of the run-time codec: a decimal
/// string, as JSON numbers lose precision past 2^53.
pub mod decimal_u64 {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse()
            .map_err(|_| D::Error::custom(format!("`{text}` is not a decimal string of a u64")))
    }
}

/// The response of `check_move`: absent `refusal` when the client may move
/// the robot, and the revision of the fleet that the answer reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckMoveResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<MoveRefusal>,
    #[serde(with = "decimal_u64")]
    pub revision: u64,
}

/// Why a client may not move a robot: the failure kind and the message that
/// the client refuses its call with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRefusal {
    pub failure_kind: RobotLeasesFailureKind,
    pub message: String,
}

/// What one feedback of `changes` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Listing,
    Join,
    Removal,
    SoftwareStop,
    SoftwareStart,
    LeaseStart,
    LeaseEnd,
    LeaseOpen,
    LeaseClose,
    OfferStart,
    OfferEnd,
    HolderChange,
    HaltStart,
    HaltEnd,
}

/// One feedback of `changes`: the change, its robot (empty for a change of
/// the whole stack), the revision of the fleet that it made, and the listing
/// after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetChange {
    pub change: ChangeKind,
    pub robot: String,
    #[serde(with = "decimal_u64")]
    pub revision: u64,
    pub listing: FleetListing,
}

/// What one feedback of `lease` reports to a lease holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEventKind {
    /// Another client offers the holder the lease of a robot.
    Offer,
    /// An offer to the holder ended.
    OfferEnd,
    /// A lease of the holder ended.
    LeaseEnd,
    /// A lease that the holder gave came back to it.
    LeaseBack,
}

/// One feedback of `lease`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseEvent {
    pub event: LeaseEventKind,
    pub robot: String,
    pub giver: String,
    pub giver_core_node: String,
    pub cause: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(text: &str) -> Result<RobotLeasesScope, String> {
        serde_json::from_str::<RobotLeasesScope>(text).map_err(|error| error.to_string())
    }

    #[test]
    fn a_scope_names_members_and_lease_brings_the_services_of_a_holder() {
        let holder = scope(r#"{ "members": ["lease", "changes"] }"#).unwrap();
        for member in [
            RobotLeasesMember::Lease,
            RobotLeasesMember::Changes,
            RobotLeasesMember::Take,
            RobotLeasesMember::Release,
            RobotLeasesMember::Offer,
            RobotLeasesMember::Open,
            RobotLeasesMember::Close,
            RobotLeasesMember::CheckMove,
        ] {
            assert!(holder.allows(member), "{member:?}");
        }
        for member in [
            RobotLeasesMember::List,
            RobotLeasesMember::Contract,
            RobotLeasesMember::Halt,
            RobotLeasesMember::ClearHalt,
        ] {
            assert!(!holder.allows(member), "{member:?}");
        }
        let controls = scope(r#"{ "members": ["list", "halt", "clear_halt"] }"#).unwrap();
        assert!(controls.allows(RobotLeasesMember::Halt));
        assert!(controls.allows(RobotLeasesMember::CheckMove));
        assert!(!controls.allows(RobotLeasesMember::Take));
        assert!(!controls.allows(RobotLeasesMember::Changes));
    }

    #[test]
    fn a_scope_refuses_an_unknown_a_repeated_or_a_lease_service_member() {
        for (text, expected) in [
            (r#"{ "members": [] }"#, "names no member"),
            (r#"{ "members": ["take"] }"#, "name `lease`"),
            (
                r#"{ "members": ["reset"] }"#,
                "not a member of `robot_leases:v1`",
            ),
            (r#"{ "members": ["list", "list"] }"#, "`list` twice"),
            (r#"{ "members": ["list"], "max": 1 }"#, "unknown field"),
        ] {
            let error = scope(text).expect_err(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn every_member_has_its_name_and_its_kind() {
        for member in RobotLeasesMember::ALL {
            assert_eq!(RobotLeasesMember::named(member.name()), Some(member));
        }
        assert_eq!(RobotLeasesMember::named("stop"), None);
        assert!(RobotLeasesMember::Changes.is_action());
        assert!(RobotLeasesMember::Lease.is_action());
        assert!(!RobotLeasesMember::Halt.is_action());
    }

    #[test]
    fn a_check_names_exactly_one_target() {
        let request = |instance: Option<&str>, robot: Option<&str>| CheckMoveRequest {
            instance: instance.map(str::to_owned),
            robot: robot.map(str::to_owned),
        };
        assert_eq!(
            MoveTarget::try_from(request(Some("alpha_backbone"), None)),
            Ok(MoveTarget::Instance("alpha_backbone".to_owned()))
        );
        assert_eq!(
            MoveTarget::try_from(request(None, Some("alpha"))),
            Ok(MoveTarget::Robot("alpha".to_owned()))
        );
        assert!(MoveTarget::try_from(request(None, None)).is_err());
        assert!(MoveTarget::try_from(request(Some("a"), Some("b"))).is_err());
    }
}
