//! The descriptions of a document as the repository cache keeps them.
//!
//! `repo refresh` parses each contract, pairing, node and launcher that a
//! repository publishes, and keeps its descriptions on its cache entry, so
//! `repo search` and `repo show` read no document: the description of the
//! document and, for a contract or a pairing, the description of each
//! member or topic with the descriptions of its fields.

use config::description::{Description, DocumentDescription};
use config::node::{InterfaceKind, MessageFormat};
use daemon_config::pairing::PeppyPairing;
use peppy_codec::contract::{MessageSide, PeppyContract};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The descriptions of one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDescriptions {
    pub description: DocumentDescription,
    /// The members of a contract and the topics of a pairing, in the order
    /// of the document. Empty for a node and a launcher.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberDescriptions>,
}

/// The description of one member of a contract or one topic of a pairing,
/// with the descriptions of its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDescriptions {
    #[serde(with = "interface_kind_name")]
    pub kind: InterfaceKind,
    pub name: String,
    pub description: Description,
    /// Each field of the message formats of the member that has a
    /// description, side by side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldDescription>,
}

/// The description of one field: the message side of its member, its path
/// in the format of that side ([`MessageFormat::field_descriptions`]) and
/// its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDescription {
    #[serde(with = "message_side_name")]
    pub side: MessageSide,
    pub path: Vec<String>,
    pub description: Description,
}

impl CachedDescriptions {
    /// The descriptions of a node or a launcher: the document has no member
    /// that the cache describes.
    pub fn of_document(description: &DocumentDescription) -> Self {
        Self {
            description: description.clone(),
            members: Vec::new(),
        }
    }

    pub fn of_contract(contract: &PeppyContract) -> Self {
        let interfaces = &contract.interfaces;
        let topics = interfaces
            .topics
            .iter()
            .map(|topic| (InterfaceKind::Topic, &topic.name, &topic.description));
        let services = interfaces
            .services
            .iter()
            .map(|service| (InterfaceKind::Service, &service.name, &service.description));
        let actions = interfaces
            .actions
            .iter()
            .map(|action| (InterfaceKind::Action, &action.name, &action.description));
        let members = topics
            .chain(services)
            .chain(actions)
            .map(|(kind, name, description)| {
                let sides = MessageSide::ALL.into_iter().filter_map(|side| {
                    contract
                        .message_format(kind, name, side)
                        .map(|format| (side, format))
                });
                MemberDescriptions {
                    kind,
                    name: name.clone(),
                    description: description.clone(),
                    fields: field_descriptions(sides),
                }
            })
            .collect();
        Self {
            description: contract.description().clone(),
            members,
        }
    }

    pub fn of_pairing(pairing: &PeppyPairing) -> Self {
        let members = pairing
            .topics
            .iter()
            .map(|topic| MemberDescriptions {
                kind: InterfaceKind::Topic,
                name: topic.name.clone(),
                description: topic.description.clone(),
                fields: field_descriptions(
                    topic
                        .message_format
                        .iter()
                        .map(|format| (MessageSide::Message, format)),
                ),
            })
            .collect();
        Self {
            description: pairing.manifest.description.clone(),
            members,
        }
    }

    /// The first paragraph of the description of the document.
    pub fn summary(&self) -> &Description {
        self.description.summary()
    }

    /// Every text of the document: its paragraphs, then the description of
    /// each member and of each of its fields.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        let paragraphs = self.description.paragraphs().iter();
        let members = self.members.iter().flat_map(|member| {
            std::iter::once(&member.description)
                .chain(member.fields.iter().map(|field| &field.description))
        });
        paragraphs.chain(members).map(Description::as_str)
    }
}

fn field_descriptions<'a>(
    sides: impl Iterator<Item = (MessageSide, &'a MessageFormat)>,
) -> Vec<FieldDescription> {
    sides
        .flat_map(|(side, format)| {
            format
                .field_descriptions()
                .into_iter()
                .map(move |(path, description)| FieldDescription {
                    side,
                    path: path.into_iter().map(str::to_owned).collect(),
                    description: description.clone(),
                })
        })
        .collect()
}

/// A member kind in the cache: `topic`, `service` or `action`.
mod interface_kind_name {
    use super::*;

    pub fn serialize<S: Serializer>(
        kind: &InterfaceKind,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(kind)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<InterfaceKind, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

/// A message side in the cache, by its name ([`MessageSide::as_str`]).
mod message_side_name {
    use super::*;

    pub fn serialize<S: Serializer>(side: &MessageSide, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(side.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<MessageSide, D::Error> {
        let name = String::deserialize(deserializer)?;
        MessageSide::parse(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown message side `{name}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::pairing::PeppyPairingParser;
    use peppy_codec::contract::PeppyContractParser;

    const ARM: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "arm", tag: "v1", description: ["An arm.", "Units are SI."] },
        interfaces: {
            topics: [{ name: "state", description: "The joints.", message_format: {
                joints: { $type: "array", $items: "f64", $description: "Radians." },
            } }],
            services: [{ name: "home", description: "Go home.", request_message_format: {
                speed: { $type: "f64", $description: "Meters per second." },
                fast: "bool",
            }, response_message_format: { moved: { $type: "bool", $description: "It moved." } } }],
            actions: [{ name: "reach", description: "Reach a pose.", goal_service: {
                response_message_format: { queue: { $type: "u32", $description: "Goals ahead." } },
            }, feedback_topic: {
                message_format: { left: { $type: "f64", $description: "Meters left." } },
            } }],
        },
    }"#;

    #[test]
    fn a_contract_keeps_each_member_with_the_descriptions_of_its_fields() {
        let contract = PeppyContractParser::from_content(ARM).unwrap();
        let cached = CachedDescriptions::of_contract(&contract);
        assert_eq!(cached.summary().as_str(), "An arm.");
        let members: Vec<(InterfaceKind, &str, &str)> = cached
            .members
            .iter()
            .map(|m| (m.kind, m.name.as_str(), m.description.as_str()))
            .collect();
        assert_eq!(
            members,
            [
                (InterfaceKind::Topic, "state", "The joints."),
                (InterfaceKind::Service, "home", "Go home."),
                (InterfaceKind::Action, "reach", "Reach a pose."),
            ]
        );
        let fields: Vec<(MessageSide, Vec<String>, &str)> = cached.members[1]
            .fields
            .iter()
            .map(|f| (f.side, f.path.clone(), f.description.as_str()))
            .collect();
        assert_eq!(
            fields,
            [
                (
                    MessageSide::Request,
                    vec!["speed".to_owned()],
                    "Meters per second."
                ),
                (MessageSide::Response, vec!["moved".to_owned()], "It moved."),
            ]
        );
        let action_sides: Vec<MessageSide> =
            cached.members[2].fields.iter().map(|f| f.side).collect();
        assert_eq!(
            action_sides,
            [MessageSide::GoalResponse, MessageSide::Feedback],
            "the goal response of an action keeps its field descriptions"
        );
        let texts: Vec<&str> = cached.texts().collect();
        assert_eq!(
            texts,
            [
                "An arm.",
                "Units are SI.",
                "The joints.",
                "Radians.",
                "Go home.",
                "Meters per second.",
                "It moved.",
                "Reach a pose.",
                "Goals ahead.",
                "Meters left.",
            ]
        );
    }

    #[test]
    fn a_pairing_keeps_each_topic() {
        let pairing = PeppyPairingParser::from_content(
            r#"{
                peppy_schema: "pairing/v1",
                manifest: { name: "grip", tag: "v1", description: ["A gripper link."] },
                roles: ["leader", "follower"],
                topics: [{ emitted_by: "leader", name: "setpoints", description: "The opening.",
                    message_format: { opening: { $type: "f64", $description: "0 closed, 1 open." } } }],
            }"#,
        )
        .unwrap();
        let cached = CachedDescriptions::of_pairing(&pairing);
        assert_eq!(cached.summary().as_str(), "A gripper link.");
        assert_eq!(cached.members[0].name, "setpoints");
        assert_eq!(cached.members[0].fields[0].side, MessageSide::Message);
        assert_eq!(cached.texts().last(), Some("0 closed, 1 open."));
    }

    #[test]
    fn the_cached_descriptions_round_trip_through_the_cache_format() {
        let contract = PeppyContractParser::from_content(ARM).unwrap();
        let cached = CachedDescriptions::of_contract(&contract);
        let text = serde_json5::to_string(&cached).unwrap();
        assert!(text.contains(r#""kind":"service""#), "{text}");
        assert!(text.contains(r#""side":"request""#), "{text}");
        let again: CachedDescriptions = serde_json5::from_str(&text).unwrap();
        assert_eq!(again, cached);
    }
}
