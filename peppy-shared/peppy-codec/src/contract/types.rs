use super::comments::{ContractComments, MessageSide};
use config::{
    contract_member::{
        ContractAction, ContractMarks, ContractMembers, ContractService, ContractTopic,
    },
    failure_kind::{ContractFailureKinds, ContractKinds},
    node::{
        InterfaceKind, LinkedMember, MessageFormat, NativeEmittedTopic, NativeExposedAction,
        NativeExposedService, RefinementProblem, SchemaType, refined_ref,
    },
    runtime::Name,
    schema::PeppySchema,
    validate_named_items,
};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};

/// Reject any `peppy_schema` value other than `contract/v1` so a node or
/// launcher document can't slip through `PeppyContractParser`.
fn deserialize_contract_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::ContractV1)
}

/// A reusable contract describing the topics, services, and actions a node
/// claims to expose. Contract documents are stand-alone JSON5 files identified
/// by `peppy_schema: "contract/v1"`; nodes claim them by name/tag in
/// `manifest.implements` and list every member as an explicit contract-backed
/// entry in their `interfaces` section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "ContractDocument", into = "ContractDocument")]
pub struct PeppyContract {
    pub peppy_schema: PeppySchema,
    pub manifest: Manifest,
    /// The failure kinds of the contract's own: its `failure_kinds` block,
    /// each kind with its meaning, and the kinds of the block that each of
    /// its services and actions lists in a `failure_kinds` list of its own.
    /// A member gives the kinds it lists, or a kind of peppy, when it
    /// refuses or fails a goal or gives an error reply.
    pub failure_kinds: ContractKinds,
    /// The marks of the members of the contract: the `marks` list of each
    /// topic, service and action.
    pub marks: ContractMarks,
    pub interfaces: Interfaces,
    /// The comments of the document, which [`super::PeppyContractParser`]
    /// reads from its text. Empty for a contract that serde reads alone.
    pub comments: ContractComments,
}

impl PeppyContract {
    /// The topic `name`, when the contract declares one.
    pub fn topic(&self, name: &str) -> Option<&NativeEmittedTopic> {
        self.interfaces
            .topics
            .iter()
            .find(|topic| topic.name == name)
    }

    /// The service `name`, when the contract declares one.
    pub fn service(&self, name: &str) -> Option<&NativeExposedService> {
        self.interfaces
            .services
            .iter()
            .find(|service| service.name == name)
    }

    /// The action `name`, when the contract declares one.
    pub fn action(&self, name: &str) -> Option<&NativeExposedAction> {
        self.interfaces
            .actions
            .iter()
            .find(|action| action.name == name)
    }

    /// The format of the message `side` of the member `name` of `kind`:
    /// `None` when the contract declares no such member, when `side` is not
    /// a side of `kind`, or when the member declares no format for it.
    pub fn message_format(
        &self,
        kind: InterfaceKind,
        name: &str,
        side: MessageSide,
    ) -> Option<&MessageFormat> {
        match (kind, side) {
            (InterfaceKind::Topic, MessageSide::Message) => {
                self.topic(name)?.message_format.as_ref()
            }
            (InterfaceKind::Service, MessageSide::Request) => {
                self.service(name)?.request_message_format.as_ref()
            }
            (InterfaceKind::Service, MessageSide::Response) => {
                self.service(name)?.response_message_format.as_ref()
            }
            (InterfaceKind::Action, MessageSide::Goal) => self
                .action(name)?
                .goal_service
                .as_ref()?
                .request_message_format
                .as_ref(),
            (InterfaceKind::Action, MessageSide::Feedback) => self
                .action(name)?
                .feedback_topic
                .as_ref()
                .map(|feedback| &feedback.message_format),
            (InterfaceKind::Action, MessageSide::Result) => self
                .action(name)?
                .result_service
                .as_ref()?
                .response_message_format
                .as_ref(),
            _ => None,
        }
    }

    /// The field at `path` of the message `side` of the member `name` of
    /// `kind`, with its enum (`$enum`) and its marks (`$marks`). A path
    /// names a field the way [`ContractComments::field`] does: the key of
    /// each object it sits in, with the items of an array passed through.
    pub fn field(
        &self,
        kind: InterfaceKind,
        name: &str,
        side: MessageSide,
        path: &[&str],
    ) -> Option<&SchemaType> {
        let (first, rest) = path.split_first()?;
        let mut field = self.message_format(kind, name, side)?.0.get(*first)?;
        for key in rest {
            field = object_field(field, key)?;
        }
        Some(field)
    }

    /// The member that a document-backed produced entry of a node manifest
    /// selects, by the entry's kind and name, with the entry's `refine`
    /// block applied. `None` when the contract declares no member of that
    /// kind by that name, `Some(Err)` with every pin of the block that the
    /// member does not admit.
    pub fn refined_member(
        &self,
        entry: LinkedMember<'_>,
    ) -> Option<Result<RefinedMember, Vec<RefinementProblem>>> {
        match entry {
            LinkedMember::Topic(entry) => self.topic(&entry.name).map(|member| {
                refined_ref(entry.refine.as_deref(), member)
                    .map(|member| RefinedMember::Topic(member.into_owned()))
            }),
            LinkedMember::Service(entry) => self.service(&entry.name).map(|member| {
                refined_ref(entry.refine.as_deref(), member)
                    .map(|member| RefinedMember::Service(member.into_owned()))
            }),
            LinkedMember::Action(entry) => self.action(&entry.name).map(|member| {
                refined_ref(entry.refine.as_deref(), member)
                    .map(|member| RefinedMember::Action(Box::new(member.into_owned())))
            }),
        }
    }
}

/// A member of a contract as a node that implements the contract produces
/// it: the contract's shape with the `refine` block of the node's entry
/// applied (see [`PeppyContract::refined_member`]).
#[derive(Debug, Clone, PartialEq)]
pub enum RefinedMember {
    Topic(NativeEmittedTopic),
    Service(NativeExposedService),
    /// Boxed for the reason [`config::node::ExposedAction`] boxes its native
    /// payload: its three optional endpoints dwarf the other two members.
    Action(Box<NativeExposedAction>),
}

/// A contract document as it is written: the `failure_kinds` list of each
/// service and action sits on the member.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractDocument {
    #[serde(deserialize_with = "deserialize_contract_v1_schema")]
    peppy_schema: PeppySchema,
    manifest: Manifest,
    #[serde(default, skip_serializing_if = "ContractFailureKinds::is_empty")]
    failure_kinds: ContractFailureKinds,
    interfaces: DocumentInterfaces,
}

/// The `interfaces` of a contract document. Each section is a flat list of
/// items; there is no `emits`/`consumes` split because a contract describes
/// the provider side only. Members carry their shape inline: a contract
/// member is the source of shape, so the contract-backed `{link_id, name}`
/// entry form node manifests use cannot appear here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentInterfaces {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_topics"
    )]
    topics: Vec<ContractTopic>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_services"
    )]
    services: Vec<ContractService>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_actions"
    )]
    actions: Vec<ContractAction>,
}

impl TryFrom<ContractDocument> for PeppyContract {
    type Error = config::failure_kind::InvalidMemberKinds;

    fn try_from(document: ContractDocument) -> Result<Self, Self::Error> {
        let ContractDocument {
            peppy_schema,
            manifest,
            failure_kinds,
            interfaces,
        } = document;
        let members = ContractMembers::new(
            failure_kinds,
            interfaces.topics,
            interfaces.services,
            interfaces.actions,
        )?;
        Ok(Self {
            peppy_schema,
            manifest,
            failure_kinds: members.failure_kinds,
            marks: members.marks,
            interfaces: Interfaces {
                topics: members.topics,
                services: members.services,
                actions: members.actions,
            },
            comments: ContractComments::default(),
        })
    }
}

impl From<PeppyContract> for ContractDocument {
    fn from(contract: PeppyContract) -> Self {
        let PeppyContract {
            peppy_schema,
            manifest,
            failure_kinds,
            marks,
            interfaces,
            comments: _,
        } = contract;
        let members = ContractMembers {
            topics: interfaces.topics,
            services: interfaces.services,
            actions: interfaces.actions,
            failure_kinds,
            marks,
        };
        Self {
            peppy_schema,
            manifest,
            failure_kinds: members.failure_kinds.declared().clone(),
            interfaces: DocumentInterfaces {
                topics: members.documented_topics(),
                services: members.documented_services(),
                actions: members.documented_actions(),
            },
        }
    }
}

/// Identity of a contract document. Contracts do not have build/runtime
/// concerns, so the manifest is narrower than a node manifest: `depends_on`
/// is rejected because a contract is passive. `labels` are
/// allowed as descriptive metadata to help discovery and filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    #[serde(deserialize_with = "deserialize_tag")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

/// Enforce the repo-ID tag contract on the manifest tag at parse time, reusing
/// the shared rules from `repo_node_id` so a `name`/`tag` pair from a contract
/// document matches node dependencies the same way everywhere.
fn deserialize_tag<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let tag = String::deserialize(deserializer)?;
    config::repo_node_id::validate_repo_node_tag(&tag, "tag").map_err(de::Error::custom)?;
    Ok(tag)
}

/// The members of a contract, each with the shape of a native member. The
/// kinds of the contract that each service and action gives are in
/// [`PeppyContract::failure_kinds`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Interfaces {
    pub topics: Vec<NativeEmittedTopic>,
    pub services: Vec<NativeExposedService>,
    pub actions: Vec<NativeExposedAction>,
}

fn deserialize_topics<'de, D>(deserializer: D) -> Result<Vec<ContractTopic>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<ContractTopic>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|t| t.name.as_str()), "topic")
        .map_err(de::Error::custom)?;
    Ok(items)
}

fn deserialize_services<'de, D>(deserializer: D) -> Result<Vec<ContractService>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<ContractService>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|s| s.name.as_str()), "service")
        .map_err(de::Error::custom)?;
    Ok(items)
}

fn deserialize_actions<'de, D>(deserializer: D) -> Result<Vec<ContractAction>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<ContractAction>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|a| a.name.as_str()), "action")
        .map_err(de::Error::custom)?;
    Ok(items)
}

/// The field `key` of the object that `field` is, or of the object that
/// each item of the array `field` is.
fn object_field<'a>(field: &'a SchemaType, key: &str) -> Option<&'a SchemaType> {
    match field {
        SchemaType::Object(object) => object.fields.get(key),
        SchemaType::Array(array) => object_field(&array.items, key),
        SchemaType::Type(_) | SchemaType::Primitive(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::failure_kind::{FailureKindName, MemberFailureKinds, MemberSection};
    use config::node::InterfaceKind;
    use config::node::{
        ArrayKind, MessageFormat, QoSProfile, SchemaType, TopicRetention, TypeToken,
    };

    /// The depth-camera example from the schema doc parses end-to-end and
    /// exposes every field the way the user wrote it.
    #[test]
    fn parses_depth_camera_example() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: {
                name: "depth_camera",
                tag: "v1"
            },
            interfaces: {
                topics: [
                    {
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {
                            header: {
                                $type: "object",
                                stamp: "time",
                                frame_id: "u32",
                            },
                            encoding: "string",
                            width: "u32",
                            height: "u32",
                            frame: {
                                $type: "array",
                                $items: "u8",
                                $length: 4
                            },
                        },
                    }
                ],
                services: [
                    {
                        name: "video_stream_info",
                        response_message_format: {
                            width: "u32",
                            height: "u32",
                            frames_per_second: "u8",
                            encoding: "string",
                        }
                    },
                    {
                        name: "set_contrast",
                        request_message_format: {
                            value: "i32",
                        },
                        response_message_format: {
                            success: "bool",
                            message: "string",
                            current_value: "i32",
                        },
                    }
                ]
            }
        }"#;

        let parsed: PeppyContract =
            serde_json5::from_str(json5).expect("depth_camera example should parse");

        assert_eq!(parsed.peppy_schema, PeppySchema::ContractV1);
        assert_eq!(parsed.manifest.name.as_str(), "depth_camera");
        assert_eq!(parsed.manifest.tag, "v1");

        assert_eq!(parsed.interfaces.topics.len(), 1);
        let topic = &parsed.interfaces.topics[0];
        assert_eq!(topic.name, "video_stream");
        assert_eq!(topic.qos_profile, QoSProfile::SensorData);
        let mf = topic.message_format.as_ref().expect("message_format set");
        assert!(matches!(
            mf.0.get("encoding"),
            Some(SchemaType::Type(TypeToken::String))
        ));
        let SchemaType::Array(frame) = mf.0.get("frame").unwrap() else {
            panic!("frame should be an array");
        };
        assert_eq!(frame.kind, ArrayKind::Array);
        assert_eq!(frame.length, Some(4));

        assert_eq!(parsed.interfaces.services.len(), 2);
        let svc = &parsed.interfaces.services[0];
        assert_eq!(svc.name, "video_stream_info");
        assert!(svc.request_message_format.is_none());
        assert!(svc.response_message_format.is_some());

        let set_contrast = &parsed.interfaces.services[1];
        assert_eq!(set_contrast.name, "set_contrast");
        assert!(set_contrast.request_message_format.is_some());
        assert!(set_contrast.response_message_format.is_some());

        assert!(parsed.interfaces.actions.is_empty());
    }

    #[test]
    fn a_contract_topic_states_its_retention() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "robot_state", tag: "v1" },
            interfaces: {
                topics: [
                    { name: "joint_state", message_format: { position: "f64" } },
                    { name: "robot_mode", retention: { latest: 1 }, message_format: { mode: "string" } },
                    { name: "calibration", retention: { latest: 5 }, message_format: { offset: "f64" } },
                ]
            }
        }"#;

        let parsed: PeppyContract = serde_json5::from_str(json5).expect("should parse");

        let latest = |depth| TopicRetention::latest(depth).unwrap();
        assert_eq!(
            parsed
                .interfaces
                .topics
                .iter()
                .map(|topic| (topic.name.as_str(), topic.retention))
                .collect::<Vec<_>>(),
            [
                ("joint_state", TopicRetention::LiveOnly),
                ("robot_mode", latest(1)),
                ("calibration", latest(5)),
            ]
        );
    }

    #[test]
    fn minimal_contract_with_only_manifest_parses() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "empty_iface", tag: "v1" },
            interfaces: {}
        }"#;

        let parsed: PeppyContract = serde_json5::from_str(json5).expect("should parse");
        assert!(parsed.interfaces.topics.is_empty());
        assert!(parsed.interfaces.services.is_empty());
        assert!(parsed.interfaces.actions.is_empty());
    }

    #[test]
    fn actions_can_be_declared() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                actions: [
                    {
                        name: "move_arm",
                        goal_service: {
                            request_message_format: { x: "f64" },
                            response_message_format: { accepted: "bool" },
                        },
                        result_service: {
                            response_message_format: { success: "bool" },
                        }
                    }
                ]
            }
        }"#;

        let parsed: PeppyContract = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(parsed.interfaces.actions.len(), 1);
        let action = &parsed.interfaces.actions[0];
        assert_eq!(action.name, "move_arm");
        assert!(action.goal_service.is_some());
        assert!(action.result_service.is_some());
        assert!(action.feedback_topic.is_none());
    }

    /// A contract must not advertise a user-defined result request: the action
    /// protocol has none, so `deny_unknown_fields` rejects it at parse time
    /// rather than letting a contract require an API no implementation or
    /// consumer can honor.
    #[test]
    fn action_rejects_result_service_request_message_format() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                actions: [
                    {
                        name: "move_arm",
                        result_service: {
                            request_message_format: { include_diagnostics: "bool" },
                            response_message_format: { success: "bool" },
                        }
                    }
                ]
            }
        }"#;

        let err = serde_json5::from_str::<PeppyContract>(json5)
            .expect_err("result_service.request_message_format must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("request_message_format"),
            "error should name the rejected field, got: {message}"
        );
    }

    /// The schema field is the source of truth: a node-shaped document
    /// must not be accepted by the contract parser, even if no
    /// contract-specific field is present.
    #[test]
    fn rejects_wrong_schema_tag() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {}
        }"#;
        let err =
            serde_json5::from_str::<PeppyContract>(json5).expect_err("node/v1 must be rejected");
        assert!(
            err.to_string().contains("contract/v1"),
            "error should mention expected schema, got: {err}"
        );
    }

    /// A contract whose block declares `unreachable` and `leader_streaming`,
    /// with the JSON5 items `services` and `actions`.
    fn motion_contract(services: &str, actions: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "contract/v1",
            manifest: {{ name: "limb_motion", tag: "v1" }},
            failure_kinds: {{
                unreachable: "No plan to the target",
                leader_streaming: "A leader streams a limb of the goal",
            }},
            interfaces: {{ services: [{services}], actions: [{actions}] }}
        }}"#
        )
    }

    #[test]
    fn reads_the_failure_kinds_of_the_contract_in_their_order() {
        let json5 = motion_contract(
            "",
            r#"{ name: "move_arm", failure_kinds: ["leader_streaming", "unreachable"] }"#,
        );
        let contract: PeppyContract = serde_json5::from_str(&json5).expect("valid contract");
        let kinds: Vec<(&str, &str)> = contract
            .failure_kinds
            .declared()
            .iter()
            .map(|(name, meaning)| (name.as_str(), meaning))
            .collect();
        assert_eq!(
            kinds,
            [
                ("unreachable", "No plan to the target"),
                ("leader_streaming", "A leader streams a limb of the goal"),
            ]
        );
    }

    /// Each member gives the kinds it lists: a service and an action of one
    /// name are two members, and a member that lists none gives the kinds
    /// of peppy only.
    #[test]
    fn each_member_gives_the_kinds_of_the_contract_that_it_lists() {
        let json5 = motion_contract(
            r#"{ name: "check_arm_move", failure_kinds: ["leader_streaming"] },
               { name: "stop" }"#,
            r#"{ name: "move_arm", failure_kinds: ["unreachable", "leader_streaming"] },
               { name: "check_arm_move" }"#,
        );
        let contract: PeppyContract = serde_json5::from_str(&json5).expect("valid contract");
        let kinds = &contract.failure_kinds;

        let move_arm = kinds.of(MemberSection::Action, "move_arm");
        assert!(move_arm.allows("unreachable") && move_arm.allows("leader_streaming"));
        let check = kinds.of(MemberSection::Service, "check_arm_move");
        assert!(check.allows("leader_streaming") && !check.allows("unreachable"));
        assert_eq!(
            kinds.of(MemberSection::Action, "check_arm_move"),
            MemberFailureKinds::peppy_only()
        );
        assert_eq!(
            kinds.of(MemberSection::Service, "stop"),
            MemberFailureKinds::peppy_only()
        );
        assert_eq!(contract.interfaces.services.len(), 2);
        assert_eq!(contract.interfaces.actions.len(), 2);
    }

    #[test]
    fn rejects_a_listed_kind_that_the_block_does_not_declare() {
        let json5 = motion_contract(
            "",
            r#"{ name: "move_arm", failure_kinds: ["unreachable", "leader_streaming", "not_found"] }"#,
        );
        let err = serde_json5::from_str::<PeppyContract>(&json5)
            .expect_err("a member lists declared kinds only");
        assert!(
            err.to_string()
                .contains("action `move_arm` lists failure kind `not_found`"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_declared_kind_that_no_member_lists() {
        let json5 = motion_contract(
            "",
            r#"{ name: "move_arm", failure_kinds: ["unreachable"] }"#,
        );
        let err = serde_json5::from_str::<PeppyContract>(&json5)
            .expect_err("each declared kind is listed");
        assert!(
            err.to_string()
                .contains("failure kind `leader_streaming` of the `failure_kinds` block is listed by no service or action"),
            "{err}"
        );
    }

    #[test]
    fn a_contract_without_failure_kinds_declares_none() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {}
        }"#;
        let contract: PeppyContract = serde_json5::from_str(json5).expect("valid contract");
        assert!(contract.failure_kinds.declared().is_empty());
    }

    #[test]
    fn rejects_a_failure_kind_of_peppy_in_the_contract() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            failure_kinds: { timeout: "Too slow" },
            interfaces: {}
        }"#;
        let err = serde_json5::from_str::<PeppyContract>(json5)
            .expect_err("a kind of peppy is not the contract's own");
        assert!(
            err.to_string().contains("`timeout` is a kind of peppy"),
            "{err}"
        );
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {},
            execution: { language: "rust" }
        }"#;
        assert!(serde_json5::from_str::<PeppyContract>(json5).is_err());
    }

    #[test]
    fn rejects_unknown_interfaces_fields() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: { mystery: [] }
        }"#;
        assert!(serde_json5::from_str::<PeppyContract>(json5).is_err());
    }

    /// Contract manifests intentionally drop `depends_on`: a contract
    /// is passive and cannot depend on other nodes.
    #[test]
    fn rejects_manifest_depends_on() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: {
                name: "x",
                tag: "v1",
                depends_on: { nodes: [] }
            },
            interfaces: {}
        }"#;
        assert!(serde_json5::from_str::<PeppyContract>(json5).is_err());
    }

    /// `labels` are descriptive metadata on the manifest, accepted so
    /// catalog tooling can filter contracts (e.g., `vendor`, `domain`)
    /// without changing the contract itself.
    #[test]
    fn accepts_manifest_labels() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: {
                name: "x",
                tag: "v1",
                labels: ["a", "b"]
            },
            interfaces: {}
        }"#;
        let parsed: PeppyContract =
            serde_json5::from_str(json5).expect("labels should be accepted");
        assert_eq!(
            parsed.manifest.labels.as_deref(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_invalid_manifest_name() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "bad/name", tag: "v1" },
            interfaces: {}
        }"#;
        assert!(serde_json5::from_str::<PeppyContract>(json5).is_err());
    }

    /// The manifest tag must satisfy the same repo-ID rules as node
    /// dependencies (`repo_node_id::validate_repo_node_tag`); a tag that does
    /// not start with an ASCII letter is rejected at parse time.
    #[test]
    fn rejects_invalid_manifest_tag() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "1bad" },
            interfaces: {}
        }"#;
        let err = serde_json5::from_str::<PeppyContract>(json5)
            .expect_err("invalid tag must be rejected");
        assert!(
            err.to_string().contains("must start with an ASCII letter"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_empty_topic_name() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                topics: [ { name: " ", qos_profile: "standard" } ]
            }
        }"#;
        let err =
            serde_json5::from_str::<PeppyContract>(json5).expect_err("empty name must be rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    #[test]
    fn rejects_duplicate_names_in_each_interface_section() {
        for (section, name) in [
            ("topics", "stream"),
            ("services", "svc"),
            ("actions", "act"),
        ] {
            let json5 = format!(
                r#"{{
                    peppy_schema: "contract/v1",
                    manifest: {{ name: "x", tag: "v1" }},
                    interfaces: {{
                        {section}: [
                            {{ name: "{name}" }},
                            {{ name: "{name}" }}
                        ]
                    }}
                }}"#
            );
            let err = serde_json5::from_str::<PeppyContract>(&json5)
                .expect_err("a duplicate name must be rejected");
            let message = err.to_string();
            assert!(
                message.contains("duplicate") && message.contains(&format!("`{name}`")),
                "{section}: {message}"
            );
        }
    }

    /// A topic with the same name as a service is allowed: the daemon
    /// addresses them through distinct namespaces, so a contract can
    /// reasonably emit `/status` while also exposing a `status` service.
    #[test]
    fn allows_same_name_across_kinds() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                topics: [ { name: "ping" } ],
                services: [ { name: "ping" } ],
                actions: [ { name: "ping" } ]
            }
        }"#;
        let parsed: PeppyContract =
            serde_json5::from_str(json5).expect("cross-kind name collisions are allowed");
        assert_eq!(parsed.interfaces.topics.len(), 1);
        assert_eq!(parsed.interfaces.services.len(), 1);
        assert_eq!(parsed.interfaces.actions.len(), 1);
    }

    /// Each topic, service and action can carry marks, which the contract
    /// keeps per member and writes back on its member.
    #[test]
    fn members_keep_their_marks_through_a_round_trip() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                topics: [{ name: "state", marks: ["robot_state"] }],
                services: [{ name: "move", marks: ["moves_robot"] }, { name: "read" }],
                actions: [{ name: "move", marks: ["moves_robot", "long_motion"] }],
            },
        }"#;
        let parsed: PeppyContract = serde_json5::from_str(json5).expect("should parse");
        assert!(parsed.marks.moves_robot(InterfaceKind::Service, "move"));
        assert!(!parsed.marks.moves_robot(InterfaceKind::Service, "read"));
        assert!(parsed.marks.moves_robot(InterfaceKind::Action, "move"));
        assert!(!parsed.marks.moves_robot(InterfaceKind::Topic, "state"));
        assert!(
            parsed
                .marks
                .of(InterfaceKind::Topic, "state")
                .contains("robot_state")
        );
        let written = serde_json5::to_string(&parsed).expect("should serialize");
        let reparsed: PeppyContract = serde_json5::from_str(&written).expect("should re-parse");
        assert_eq!(parsed, reparsed);

        for (members, expected) in [
            (r#"services: [{ name: "a", marks: [] }]"#, "leave it out"),
            (
                r#"actions: [{ name: "a", marks: ["moves_robot", "moves_robot"] }]"#,
                "twice",
            ),
            (
                r#"topics: [{ name: "a", marks: ["Moves"] }]"#,
                "invalid mark name",
            ),
        ] {
            let json5 = format!(
                r#"{{ peppy_schema: "contract/v1", manifest: {{ name: "x", tag: "v1" }},
                    interfaces: {{ {members} }} }}"#
            );
            let error = serde_json5::from_str::<PeppyContract>(&json5)
                .expect_err(members)
                .to_string();
            assert!(error.contains(expected), "{members}: {error}");
        }
    }

    /// A contract of an arm, with an enum, marks of fields and members,
    /// and comments on the document, a member and a field.
    const MARKED_ARM: &str = r#"// An arm with a gripper.
    {
        peppy_schema: "contract/v1",
        manifest: { name: "arm", tag: "v1" },
        interfaces: {
            topics: [
                {
                    name: "state",
                    message_format: {
                        // The mode of the arm.
                        mode: { $type: "string", $enum: ["idle", "moving"] },
                        joints: { $type: "array", $items: { $type: "object", angle: { $type: "f64", $marks: ["joint_angle"] } } },
                    },
                },
            ],
            actions: [
                // Moves the gripper to a pose.
                {
                    name: "move_gripper",
                    marks: ["moves_robot", "long_motion"],
                    goal_service: {
                        request_message_format: {
                            target: {
                                $type: "object",
                                // The item to grip.
                                item: { $type: "string", $marks: ["item_id"] },
                            },
                        },
                    },
                    feedback_topic: { message_format: { progress: "f32" } },
                },
            ],
        },
    }"#;

    /// The parser API gives the enum of a field, the marks of a field and
    /// of a member, and the comments of the document, a member and a field.
    #[test]
    fn the_parser_gives_enums_marks_and_comments() {
        let contract = super::super::PeppyContractParser::from_content(MARKED_ARM)
            .expect("the contract parses");

        let mode = contract
            .field(
                InterfaceKind::Topic,
                "state",
                MessageSide::Message,
                &["mode"],
            )
            .expect("the field `mode`");
        assert_eq!(
            mode.enum_values().map(|values| values.values()),
            Some(["idle".to_owned(), "moving".to_owned()].as_slice())
        );
        assert!(mode.marks().is_empty());

        let angle = contract
            .field(
                InterfaceKind::Topic,
                "state",
                MessageSide::Message,
                &["joints", "angle"],
            )
            .expect("an item of an array is passed through");
        assert!(angle.marks().contains("joint_angle"));
        assert!(angle.enum_values().is_none());

        let item = contract
            .field(
                InterfaceKind::Action,
                "move_gripper",
                MessageSide::Goal,
                &["target", "item"],
            )
            .expect("the field `target.item`");
        assert!(item.marks().contains("item_id"));

        let marks = contract.marks.of(InterfaceKind::Action, "move_gripper");
        let names: Vec<&str> = marks.iter().map(|mark| mark.as_str()).collect();
        assert_eq!(names, ["moves_robot", "long_motion"]);
        assert!(
            contract
                .marks
                .moves_robot(InterfaceKind::Action, "move_gripper")
        );
        assert!(contract.marks.of(InterfaceKind::Topic, "state").is_empty());

        assert_eq!(contract.comments.document(), Some("An arm with a gripper."));
        assert_eq!(
            contract
                .comments
                .member(InterfaceKind::Action, "move_gripper"),
            Some("Moves the gripper to a pose.")
        );
        assert_eq!(
            contract.comments.field(
                InterfaceKind::Topic,
                "state",
                MessageSide::Message,
                &["mode"]
            ),
            Some("The mode of the arm.")
        );
        assert_eq!(
            contract.comments.field(
                InterfaceKind::Action,
                "move_gripper",
                MessageSide::Goal,
                &["target", "item"]
            ),
            Some("The item to grip.")
        );
    }

    /// A message format is found by the kind, the name and the side of its
    /// member; a side of another kind, a member the contract does not
    /// declare and a path into a scalar give none.
    #[test]
    fn a_message_format_and_a_field_are_found_by_their_member_and_side() {
        let contract = super::super::PeppyContractParser::from_content(MARKED_ARM)
            .expect("the contract parses");
        let feedback = contract
            .message_format(InterfaceKind::Action, "move_gripper", MessageSide::Feedback)
            .expect("the feedback format");
        assert!(feedback.0.contains_key("progress"));
        assert!(
            contract
                .message_format(InterfaceKind::Action, "move_gripper", MessageSide::Result)
                .is_none(),
            "the action declares no result"
        );
        assert!(
            contract
                .message_format(InterfaceKind::Action, "move_gripper", MessageSide::Request)
                .is_none(),
            "an action has no request side"
        );
        assert!(
            contract
                .message_format(InterfaceKind::Service, "move_gripper", MessageSide::Request)
                .is_none(),
            "the contract declares no service of that name"
        );
        assert!(
            contract
                .field(
                    InterfaceKind::Topic,
                    "state",
                    MessageSide::Message,
                    &["mode", "x"]
                )
                .is_none(),
            "a string has no field"
        );
        assert!(
            contract
                .field(InterfaceKind::Topic, "state", MessageSide::Message, &[])
                .is_none()
        );
        assert!(contract.topic("state").is_some());
        assert!(contract.action("move_gripper").is_some());
        assert!(contract.service("state").is_none());
    }

    /// The parser reads the comments of the text beside the document.
    #[test]
    fn the_parser_keeps_the_comments_of_the_text() {
        let json5 = r#"// An arm.
        {
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                services: [
                    // Moves the arm.
                    { name: "move", request_message_format: {
                        // The target, in meters.
                        x: "f64",
                    } },
                ],
            },
        }"#;
        let parsed = super::super::PeppyContractParser::from_content(json5).expect("should parse");
        assert_eq!(parsed.comments.document(), Some("An arm."));
        assert_eq!(
            parsed.comments.member(InterfaceKind::Service, "move"),
            Some("Moves the arm.")
        );
        assert_eq!(
            parsed.comments.field(
                InterfaceKind::Service,
                "move",
                super::super::MessageSide::Request,
                &["x"]
            ),
            Some("The target, in meters.")
        );
    }

    /// Round-tripping survives canonicalization: the parsed AST serializes
    /// back to a form that re-parses to the same AST.
    #[test]
    fn round_trips_through_serde() {
        let original = PeppyContract {
            peppy_schema: PeppySchema::ContractV1,
            manifest: Manifest {
                name: Name::new("camera").unwrap(),
                tag: "v123".to_string(),
                labels: Some(vec!["vendor".to_string(), "sensor".to_string()]),
            },
            failure_kinds: ContractKinds::new(
                ContractFailureKinds::new([(
                    FailureKindName::parse("lens_covered").unwrap(),
                    "The lens is covered".to_string(),
                )])
                .unwrap(),
                [(
                    MemberSection::Service,
                    "capture".to_owned(),
                    vec![FailureKindName::parse("lens_covered").unwrap()],
                )],
            )
            .unwrap(),
            marks: ContractMarks::default(),
            comments: ContractComments::default(),
            interfaces: Interfaces {
                topics: vec![NativeEmittedTopic {
                    name: "stream".to_string(),
                    qos_profile: QoSProfile::SensorData,
                    retention: TopicRetention::latest(5).unwrap(),
                    message_format: Some(MessageFormat(
                        [("width".to_string(), SchemaType::Type(TypeToken::U32))]
                            .into_iter()
                            .collect(),
                    )),
                }],
                services: vec![NativeExposedService {
                    name: "capture".to_string(),
                    request_message_format: None,
                    response_message_format: None,
                }],
                actions: vec![],
            },
        };

        let serialized = serde_json5::to_string(&original).expect("serialize");
        let reparsed: PeppyContract = serde_json5::from_str(&serialized).expect("re-parse");
        assert_eq!(original, reparsed);
    }
}
