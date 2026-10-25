//! The interfaces of one running instance, read from its pinned documents:
//! the members of the contracts its node implements, the members the node
//! declares itself, and the topics of the pairings it plays a role of, each
//! with the message formats of its pinned manifest (a contract's formats
//! with the `refine` blocks of the manifest applied). `peppy node call`,
//! `echo` and `describe` read this model, never the code of a node.

use crate::commands::{CALLER_INSTANCE_ID, request_failure_text};
use crate::context::DaemonConnection;
use crate::error::Error;
use config::RefinementMismatch;
use config::failure_kind::{MemberFailureKinds, MemberSection};
use config::node::{
    InterfaceKind, LinkedMember, MessageFormat, NativeEmittedTopic, NativeExposedAction,
    NativeExposedService, NodeConfig, NodeConfigParser, refined,
};
use core_node_api::encoding::InstanceDocumentsRequest;
use core_node_api::encoding::{InstanceDocuments, PinnedDocument};
use daemon_config::contract::{PeppyContract, PeppyContractParser, RefinedMember};
use daemon_config::pairing::{PeppyPairing, PeppyPairingParser};
use message_codec::MessageCodec;
use message_codec::consumer::ConsumerIdentity;
use peppylib::messaging::{ProducerRef, SenderTarget};
use serde::Serialize;
use serde::ser::{SerializeSeq, Serializer};
use std::time::Duration;

/// How long the daemon has to give the documents of an instance. A pinned
/// document whose copy on the daemon's machine is gone is fetched from its
/// origin first. The daemon gives up on a read after 100 s, before this
/// budget ends, and says so in its reply, unless the request waited behind
/// the read of another request.
const DOCUMENTS_TIMEOUT: Duration = Duration::from_secs(120);

/// The interfaces of the instance `instance_id` of the daemon that `conn`
/// targets, read from the pinned documents the daemon gives. An instance
/// that the daemon does not run, or documents it cannot give, are errors of
/// the command.
pub(crate) async fn fetch(
    conn: &DaemonConnection<'_>,
    instance_id: &str,
) -> crate::error::Result<InstanceInterfaces> {
    let doing = format!("Failed to read the documents of instance `{instance_id}`");
    let documents = peppylib::core_node::transport::poll(
        &InstanceDocumentsRequest::new(instance_id),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        DOCUMENTS_TIMEOUT,
    )
    .await
    .map_err(|error| Error::ExecutionFailed(request_failure_text(&doing, &error)))?;
    InstanceInterfaces::from_documents(&documents)
        .map_err(|error| Error::ExecutionFailed(error.to_string()))
}

/// The interfaces of one instance, grouped by where each member comes from.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InstanceInterfaces {
    pub instance_id: String,
    /// The daemon that runs the instance.
    pub core_node: String,
    /// The node, by its pinned manifest.
    pub node: DocumentIdentity,
    pub contracts: Vec<ContractInterfaces>,
    pub own_members: Vec<Member>,
    pub pairings: Vec<PairingInterfaces>,
}

/// A pinned document: its name, its tag and the SHA-256 of its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DocumentIdentity {
    pub name: String,
    pub tag: String,
    pub sha256: String,
}

impl From<&PinnedDocument> for DocumentIdentity {
    fn from(document: &PinnedDocument) -> Self {
        Self {
            name: document.name().to_owned(),
            tag: document.tag().to_owned(),
            sha256: document.sha256().to_owned(),
        }
    }
}

/// The members of one contract that the node implements through its slot
/// `link_id`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContractInterfaces {
    #[serde(flatten)]
    pub contract: DocumentIdentity,
    pub link_id: String,
    pub members: Vec<Member>,
}

/// The topics the node emits through its pairing slot `link_id`, where it
/// plays `role`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PairingInterfaces {
    #[serde(flatten)]
    pub pairing: DocumentIdentity,
    pub link_id: String,
    pub role: String,
    pub topics: Vec<Member>,
}

/// One topic, service or action, with the message formats of the pinned
/// manifest, in the shape a native entry of a manifest has.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Member {
    Topic(NativeEmittedTopic),
    Service(ServiceMember),
    Action(ActionMember),
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ServiceMember {
    #[serde(flatten)]
    pub service: NativeExposedService,
    /// The kinds of its contract that the service gives, beside the kinds
    /// of peppy.
    #[serde(serialize_with = "contract_kinds")]
    pub failure_kinds: MemberFailureKinds,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActionMember {
    #[serde(flatten)]
    pub action: NativeExposedAction,
    /// The kinds of its contract that the action gives, beside the kinds of
    /// peppy.
    #[serde(serialize_with = "contract_kinds")]
    pub failure_kinds: MemberFailureKinds,
}

/// The kinds of the contract that a member gives, each with its meaning, in
/// the order of the contract. The kinds of peppy, which every member can
/// give, are not listed.
fn contract_kinds<S: Serializer>(
    kinds: &MemberFailureKinds,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Kind<'a> {
        kind: &'a str,
        meaning: &'a str,
    }
    let contract = kinds.contract_kinds();
    let mut seq = serializer.serialize_seq(Some(contract.iter().count()))?;
    for (kind, meaning) in contract.iter() {
        seq.serialize_element(&Kind {
            kind: kind.as_str(),
            meaning,
        })?;
    }
    seq.end()
}

impl Member {
    pub fn name(&self) -> &str {
        match self {
            Member::Topic(topic) => &topic.name,
            Member::Service(service) => &service.service.name,
            Member::Action(action) => &action.action.name,
        }
    }

    pub fn kind(&self) -> InterfaceKind {
        match self {
            Member::Topic(_) => InterfaceKind::Topic,
            Member::Service(_) => InterfaceKind::Service,
            Member::Action(_) => InterfaceKind::Action,
        }
    }
}

/// How the wire addresses a member of the instance, besides its name: the
/// target it is served under, and for a pairing topic the slot it is
/// emitted through.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MemberAddress {
    /// A contract member or an own member, under `target`.
    Interface { target: SenderTarget },
    /// A pairing topic, under `target`, emitted through the slot `link_id`.
    Pairing {
        target: SenderTarget,
        link_id: String,
    },
}

/// A member of the instance that a name selected, with how to reach it.
#[derive(Debug, Clone)]
pub(crate) struct SelectedMember<'a> {
    /// The member as `<qualifier>.<name>`: the contract, the node or the
    /// pairing it belongs to, then its name.
    pub qualified_name: String,
    pub member: &'a Member,
    pub address: MemberAddress,
    pub producer: ProducerRef,
}

impl SelectedMember<'_> {
    /// The codec of the message side `side` of the member, laid out from
    /// `format`, the format of the pinned manifest.
    pub fn codec(&self, side: &str, format: &MessageFormat) -> crate::error::Result<MessageCodec> {
        let label: String = format!("{}_{side}", self.qualified_name)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        MessageCodec::new(&label, format.clone())
            .map_err(|error| Error::ExecutionFailed(format!("`{}`: {error}", self.qualified_name)))
    }
}

/// Who `peppy node call` and `echo` are on the wire: the core node of the
/// local daemon, and the instance id of the command line. Each process is a
/// sender of its own, so a provider ends the goals of a command whose
/// process ends, and of no other.
pub(crate) fn command_line_caller(conn: &DaemonConnection<'_>) -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: conn.core_node_name.clone(),
        instance_id: CALLER_INSTANCE_ID.to_owned(),
    }
}

/// Why the pinned documents of an instance do not give its interfaces.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DocumentsError {
    #[error("the manifest of instance `{instance}` does not parse: {reason}")]
    Manifest { instance: String, reason: String },
    #[error("{kind} `{name}:{tag}` of instance `{instance}` does not parse: {reason}")]
    Document {
        instance: String,
        kind: &'static str,
        name: String,
        tag: String,
        reason: String,
    },
    #[error(
        "the daemon gave no {kind} `{name}:{tag}`, which the manifest of instance `{instance}` names"
    )]
    Missing {
        instance: String,
        kind: &'static str,
        name: String,
        tag: String,
    },
    #[error(
        "the entry `{name}` ({entry_kind}) of slot `{link_id}` of instance `{instance}` does not \
         match its pinned document: {reason}"
    )]
    Entry {
        instance: String,
        link_id: String,
        name: String,
        entry_kind: InterfaceKind,
        reason: String,
    },
}

impl InstanceInterfaces {
    /// The interfaces of the instance whose pinned documents are
    /// `documents`.
    pub fn from_documents(documents: &InstanceDocuments) -> Result<Self, DocumentsError> {
        let instance = documents.instance_id.as_str();
        let config = NodeConfigParser::from_content(documents.manifest.content()).map_err(|e| {
            DocumentsError::Manifest {
                instance: instance.to_owned(),
                reason: e.to_string(),
            }
        })?;
        let contracts = parse_documents(instance, "contract", &documents.contracts, |content| {
            PeppyContractParser::from_content(content).map_err(|e| e.to_string())
        })?;
        let pairings = parse_documents(instance, "pairing", &documents.pairings, |content| {
            PeppyPairingParser::from_content(content).map_err(|e| e.to_string())
        })?;
        let builder = Builder {
            instance,
            config: &config,
            contracts: &contracts,
            pairings: &pairings,
        };
        Ok(Self {
            instance_id: documents.instance_id.clone(),
            core_node: documents.core_node.clone(),
            node: DocumentIdentity::from(&documents.manifest),
            contracts: builder.contract_interfaces()?,
            own_members: builder.own_members(),
            pairings: builder.pairing_interfaces()?,
        })
    }

    /// The member that `name` selects among the members of the kinds
    /// `kinds`: `<contract>.<member>`, `<pairing>.<topic>`,
    /// `<node>.<member>` for a member the node declares itself, or the name
    /// alone when one member of those kinds has it. A name that selects no
    /// member, or several, is refused with the names that it could mean.
    pub fn select(
        &self,
        name: &str,
        kinds: &[InterfaceKind],
    ) -> Result<SelectedMember<'_>, String> {
        let candidates: Vec<SelectedMember<'_>> = self
            .members()?
            .into_iter()
            .filter(|candidate| kinds.contains(&candidate.member.kind()))
            .collect();
        let matches: Vec<&SelectedMember<'_>> = match name.split_once('.') {
            Some(_) => candidates
                .iter()
                .filter(|candidate| candidate.qualified_name == name)
                .collect(),
            None => candidates
                .iter()
                .filter(|candidate| candidate.member.name() == name)
                .collect(),
        };
        let what = kind_list(kinds);
        match matches.as_slice() {
            [selected] => Ok((*selected).clone()),
            [] => Err(format!(
                "instance `{}` has no {what} `{name}`; its {what}s: {}",
                self.instance_id,
                name_list(candidates.iter())
            )),
            several => Err(format!(
                "`{name}` names more than one {what} of instance `{}`: {}; name one of them",
                self.instance_id,
                name_list(several.iter().copied())
            )),
        }
    }

    /// Every member of the instance, with its qualified name and how the
    /// wire reaches it: the contract members, then the own members, then
    /// the pairing topics.
    fn members<'s>(&'s self) -> Result<Vec<SelectedMember<'s>>, String> {
        let producer = ProducerRef::new(self.core_node.clone(), self.instance_id.clone());
        let selected = |qualifier: &str, member: &'s Member, address| SelectedMember {
            qualified_name: format!("{qualifier}.{}", member.name()),
            member,
            address,
            producer: producer.clone(),
        };
        let invalid = |e: peppylib::messaging::SenderTargetError| e.to_string();
        let mut members = Vec::new();
        for contract in &self.contracts {
            let target = SenderTarget::contract(&contract.contract.name, &contract.contract.tag)
                .map_err(invalid)?;
            for member in &contract.members {
                let address = MemberAddress::Interface {
                    target: target.clone(),
                };
                members.push(selected(&contract.contract.name, member, address));
            }
        }
        let target = SenderTarget::node(&self.node.name, &self.node.tag).map_err(invalid)?;
        for member in &self.own_members {
            let address = MemberAddress::Interface {
                target: target.clone(),
            };
            members.push(selected(&self.node.name, member, address));
        }
        for pairing in &self.pairings {
            let target = SenderTarget::pairing(&pairing.pairing.name, &pairing.pairing.tag)
                .map_err(invalid)?;
            for topic in &pairing.topics {
                let address = MemberAddress::Pairing {
                    target: target.clone(),
                    link_id: pairing.link_id.clone(),
                };
                members.push(selected(&pairing.pairing.name, topic, address));
            }
        }
        Ok(members)
    }
}

/// "topic", "service or action": the kinds a name may select.
fn kind_list(kinds: &[InterfaceKind]) -> String {
    kinds
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The qualified names of `members`, or "none".
fn name_list<'a, 'b: 'a>(members: impl Iterator<Item = &'a SelectedMember<'b>>) -> String {
    let names: Vec<&str> = members.map(|m| m.qualified_name.as_str()).collect();
    if names.is_empty() {
        return "none".to_owned();
    }
    names.join(", ")
}

/// Each document of `documents` parsed with `parse`, with its identity.
fn parse_documents<T>(
    instance: &str,
    kind: &'static str,
    documents: &[PinnedDocument],
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Vec<(DocumentIdentity, T)>, DocumentsError> {
    documents
        .iter()
        .map(|document| {
            let parsed = parse(document.content()).map_err(|reason| DocumentsError::Document {
                instance: instance.to_owned(),
                kind,
                name: document.name().to_owned(),
                tag: document.tag().to_owned(),
                reason,
            })?;
            Ok((DocumentIdentity::from(document), parsed))
        })
        .collect()
}

/// Builds the groups of [`InstanceInterfaces`] from a parsed manifest and
/// its parsed documents.
struct Builder<'a> {
    instance: &'a str,
    config: &'a NodeConfig,
    contracts: &'a [(DocumentIdentity, PeppyContract)],
    pairings: &'a [(DocumentIdentity, PeppyPairing)],
}

impl Builder<'_> {
    /// One group per `manifest.implements` slot, holding the members its
    /// entries select, in manifest order.
    fn contract_interfaces(&self) -> Result<Vec<ContractInterfaces>, DocumentsError> {
        let mut groups = Vec::with_capacity(self.config.manifest.implements.len());
        for slot in &self.config.manifest.implements {
            let (identity, contract) =
                self.document(self.contracts, "contract", slot.name.as_str(), &slot.tag)?;
            let mut members = Vec::new();
            for entry in self.config.interfaces.linked_entries() {
                if entry.link_id() != slot.link_id {
                    continue;
                }
                members.push(self.contract_member((identity, contract), &slot.link_id, entry)?);
            }
            groups.push(ContractInterfaces {
                contract: identity.clone(),
                link_id: slot.link_id.clone(),
                members,
            });
        }
        Ok(groups)
    }

    /// The member of `contract` that `entry` selects, refined as the entry
    /// says, with the kinds of the contract it gives.
    fn contract_member(
        &self,
        contract: (&DocumentIdentity, &PeppyContract),
        link_id: &str,
        entry: LinkedMember<'_>,
    ) -> Result<Member, DocumentsError> {
        let (identity, contract) = contract;
        let mismatch = |reason: String| DocumentsError::Entry {
            instance: self.instance.to_owned(),
            link_id: link_id.to_owned(),
            name: entry.name().to_owned(),
            entry_kind: entry.kind(),
            reason,
        };
        let refined = contract
            .refined_member(entry)
            .ok_or_else(|| mismatch("the contract declares no such member".to_owned()))?
            .map_err(|problems| {
                let pinned = (identity.name.as_str(), identity.tag.as_str());
                mismatch(
                    RefinementMismatch::for_contract(pinned, link_id, entry.name(), problems)
                        .to_string(),
                )
            })?;
        let kinds = &contract.failure_kinds;
        Ok(match refined {
            RefinedMember::Topic(topic) => Member::Topic(topic),
            RefinedMember::Service(service) => Member::Service(ServiceMember {
                failure_kinds: kinds.of(MemberSection::Service, &service.name),
                service,
            }),
            RefinedMember::Action(action) => Member::Action(ActionMember {
                failure_kinds: kinds.of(MemberSection::Action, &action.name),
                action: *action,
            }),
        })
    }

    /// The members the node declares itself, topics then services then
    /// actions, in manifest order. They give the kinds of peppy only.
    fn own_members(&self) -> Vec<Member> {
        let interfaces = &self.config.interfaces;
        let topics = interfaces.native_emits().cloned().map(Member::Topic);
        let services = interfaces.native_service_exposes().map(|service| {
            Member::Service(ServiceMember {
                service: service.clone(),
                failure_kinds: MemberFailureKinds::peppy_only(),
            })
        });
        let actions = interfaces.native_action_exposes().map(|action| {
            Member::Action(ActionMember {
                action: action.clone(),
                failure_kinds: MemberFailureKinds::peppy_only(),
            })
        });
        topics.chain(services).chain(actions).collect()
    }

    /// One group per `depends_on.pairings` slot, holding the topics the
    /// node emits through it, in manifest order. An observer slot emits
    /// nothing, so it has no group.
    fn pairing_interfaces(&self) -> Result<Vec<PairingInterfaces>, DocumentsError> {
        let Some(depends_on) = self.config.manifest.depends_on.as_ref() else {
            return Ok(Vec::new());
        };
        let mut groups = Vec::with_capacity(depends_on.pairings.len());
        for slot in &depends_on.pairings {
            let (identity, pairing) =
                self.document(self.pairings, "pairing", slot.name.as_str(), &slot.tag)?;
            let mut topics = Vec::new();
            for entry in self.config.interfaces.linked_entries() {
                let LinkedMember::Topic(topic_entry) = entry else {
                    continue;
                };
                if topic_entry.link_id != slot.link_id {
                    continue;
                }
                let mismatch = |reason: String| DocumentsError::Entry {
                    instance: self.instance.to_owned(),
                    link_id: slot.link_id.clone(),
                    name: topic_entry.name.clone(),
                    entry_kind: InterfaceKind::Topic,
                    reason,
                };
                let topic = pairing
                    .topics
                    .iter()
                    .find(|topic| topic.name == topic_entry.name)
                    .ok_or_else(|| mismatch("the pairing declares no such topic".to_owned()))?;
                let native = refined(topic_entry.refine.as_deref(), topic.as_native()).map_err(
                    |problems| {
                        let pinned = (identity.name.as_str(), identity.tag.as_str());
                        mismatch(
                            RefinementMismatch::for_pairing(
                                pinned,
                                &slot.link_id,
                                &topic_entry.name,
                                problems,
                            )
                            .to_string(),
                        )
                    },
                )?;
                topics.push(Member::Topic(native));
            }
            groups.push(PairingInterfaces {
                pairing: identity.clone(),
                link_id: slot.link_id.clone(),
                role: slot.role.clone(),
                topics,
            });
        }
        Ok(groups)
    }

    /// The pinned document `name:tag` of `documents`.
    fn document<'d, T>(
        &self,
        documents: &'d [(DocumentIdentity, T)],
        kind: &'static str,
        name: &str,
        tag: &str,
    ) -> Result<&'d (DocumentIdentity, T), DocumentsError> {
        documents
            .iter()
            .find(|(identity, _)| identity.name == name && identity.tag == tag)
            .ok_or_else(|| DocumentsError::Missing {
                instance: self.instance.to_owned(),
                kind,
                name: name.to_owned(),
                tag: tag.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::{CORE_NODE, INSTANCE, documents};
    use super::*;
    use serde_json::json;

    fn interfaces() -> InstanceInterfaces {
        InstanceInterfaces::from_documents(&documents()).expect("the documents give the interfaces")
    }

    /// Each member comes with the formats of the pinned manifest: the
    /// contract's, with the `refine` block of the manifest applied, and the
    /// kinds of its contract that it gives.
    #[test]
    fn the_documents_give_each_member_with_the_formats_of_the_pinned_manifest() {
        let described = serde_json::to_value(interfaces()).expect("the model serializes");
        let documents = documents();
        assert_eq!(described["instance_id"], INSTANCE);
        assert_eq!(described["core_node"], CORE_NODE);
        assert_eq!(
            described["node"],
            json!({"name": "arm", "tag": "v1", "sha256": documents.manifest.sha256()})
        );

        let contract = &described["contracts"][0];
        assert_eq!(contract["name"], "limb_motion");
        assert_eq!(contract["sha256"], documents.contracts[0].sha256());
        assert_eq!(contract["link_id"], "arm");
        let members = contract["members"].as_array().expect("a list of members");
        let kinds_and_names: Vec<(&str, &str)> = members
            .iter()
            .map(|m| (m["kind"].as_str().unwrap(), m["name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            kinds_and_names,
            [
                ("topic", "limb_states"),
                ("service", "check_move"),
                ("action", "move_to")
            ]
        );
        assert_eq!(
            members[2]["goal_service"]["request_message_format"]["joints"],
            json!({"$type": "array", "$items": "f64", "$length": 3}),
            "the refine block of the manifest pins the length"
        );
        assert_eq!(
            members[1]["request_message_format"]["joints"],
            json!({"$type": "array", "$items": "f64"}),
            "a member the manifest does not refine keeps the contract's format"
        );
        let unreachable = json!([{"kind": "unreachable", "meaning": "No plan to the target"}]);
        assert_eq!(members[1]["failure_kinds"], unreachable);
        assert_eq!(members[2]["failure_kinds"], unreachable);

        let own = described["own_members"]
            .as_array()
            .expect("a list of members");
        assert_eq!(own[0]["kind"], "topic");
        assert_eq!(own[0]["name"], "heartbeat");
        assert_eq!(own[1]["kind"], "service");
        assert_eq!(own[1]["name"], "set_gain");
        assert_eq!(own[1]["failure_kinds"], json!([]));

        let pairing = &described["pairings"][0];
        assert_eq!(pairing["name"], "arm_link");
        assert_eq!(pairing["link_id"], "controller");
        assert_eq!(pairing["role"], "arm");
        assert_eq!(pairing["topics"][0]["name"], "joint_states");
        assert_eq!(pairing["topics"][0]["qos_profile"], "sensor_data");
        assert_eq!(
            pairing["topics"].as_array().map(Vec::len),
            Some(1),
            "the node emits only the topics of its role"
        );
    }

    /// A name selects a member by `<qualifier>.<name>`, or by its name alone
    /// when it is unique, with the target and the producer the wire reaches
    /// it at.
    #[test]
    fn a_name_selects_one_member_with_its_address() {
        let interfaces = interfaces();
        let producer = ProducerRef::new(CORE_NODE.to_owned(), INSTANCE.to_owned());
        let calls = [InterfaceKind::Service, InterfaceKind::Action];

        let move_to = interfaces.select("move_to", &calls).expect("unique");
        assert_eq!(move_to.qualified_name, "limb_motion.move_to");
        assert_eq!(
            move_to.address,
            MemberAddress::Interface {
                target: SenderTarget::contract("limb_motion", "v1").unwrap()
            }
        );
        assert_eq!(move_to.producer, producer);

        let set_gain = interfaces
            .select("arm.set_gain", &calls)
            .expect("qualified");
        assert_eq!(
            set_gain.address,
            MemberAddress::Interface {
                target: SenderTarget::node("arm", "v1").unwrap()
            }
        );

        let joint_states = interfaces
            .select("joint_states", &[InterfaceKind::Topic])
            .expect("unique");
        assert_eq!(joint_states.qualified_name, "arm_link.joint_states");
        assert_eq!(
            joint_states.address,
            MemberAddress::Pairing {
                target: SenderTarget::pairing("arm_link", "v1").unwrap(),
                link_id: "controller".to_owned(),
            }
        );
    }

    #[test]
    fn a_name_that_selects_no_member_of_the_kinds_lists_the_members_it_could_mean() {
        let error = interfaces()
            .select("move_to", &[InterfaceKind::Topic])
            .expect_err("move_to is an action");
        assert_eq!(
            error,
            "instance `arm_inst` has no topic `move_to`; its topics: limb_motion.limb_states, \
             arm.heartbeat, arm_link.joint_states"
        );
        assert!(
            interfaces()
                .select("arm.move_to", &[InterfaceKind::Action])
                .is_err(),
            "a contract member is not an own member"
        );
    }

    #[test]
    fn a_name_alone_that_two_members_have_is_refused_with_both_names() {
        let mut interfaces = interfaces();
        let contract_service = interfaces.contracts[0].members[1].clone();
        interfaces.own_members.push(contract_service);
        let error = interfaces
            .select("check_move", &[InterfaceKind::Service])
            .expect_err("two services are called check_move");
        assert_eq!(
            error,
            "`check_move` names more than one service of instance `arm_inst`: \
             limb_motion.check_move, arm.check_move; name one of them"
        );
        assert!(
            interfaces
                .select("arm.check_move", &[InterfaceKind::Service])
                .is_ok(),
            "the qualified name selects one"
        );
    }

    #[test]
    fn a_manifest_that_names_a_document_the_daemon_did_not_give_is_refused() {
        let mut documents = documents();
        documents.pairings.clear();
        let error = InstanceInterfaces::from_documents(&documents).expect_err("no pairing");
        assert_eq!(
            error.to_string(),
            "the daemon gave no pairing `arm_link:v1`, which the manifest of instance `arm_inst` \
             names"
        );
    }
}
