use config::validate_named_items;
use config::{
    node::{MessageFormat, NativeEmittedTopic, QoSProfile, TopicRetention},
    runtime::Name,
    schema::PeppySchema,
};
use peppy_codec::contract::Manifest;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};

/// Reject any `peppy_schema` value other than `pairing/v1` so a node,
/// launcher, or contract document can't slip through `PeppyPairingParser`.
fn deserialize_pairing_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::PairingV1)
}

/// A named, two-role, topics-only contract: one document describes the whole
/// conversation between two node instances that pair 1:1 over it. Pairing
/// documents are stand-alone JSON5 files identified by
/// `peppy_schema: "pairing/v1"`; nodes reference them via
/// `depends_on.pairings`, declaring which of the two roles they play.
///
/// Direction is explicit data on each topic: a role emits the topics tagged
/// `emitted_by: <its name>` and consumes every topic tagged with the other
/// role's name. There is no nesting and no convention: the flat `topics`
/// list with per-topic `emitted_by` is the entire shape.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PeppyPairing {
    pub peppy_schema: PeppySchema,
    pub manifest: Manifest,
    /// Exactly two distinct role names.
    pub roles: Vec<Name>,
    /// The whole conversation, flat. Topic names are unique across the list
    /// (one namespace regardless of direction), which is what keeps the
    /// generated `pairings.<link_id>.<topic>` module namespace unambiguous.
    pub topics: Vec<PairingTopic>,
}

impl PeppyPairing {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.as_str() == role)
    }
}

/// One topic of the conversation: its name, QoS and shape, plus `emitted_by`,
/// which names the emitting role; the other role consumes the topic. A pairing
/// topic is live only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawPairingTopic")]
pub struct PairingTopic {
    pub emitted_by: String,
    pub name: String,
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
}

impl PairingTopic {
    /// The topic with the shape of a native topic: its QoS and its message
    /// format, live only, as every pairing topic is.
    pub fn as_native(&self) -> NativeEmittedTopic {
        NativeEmittedTopic {
            name: self.name.clone(),
            qos_profile: self.qos_profile.clone(),
            retention: TopicRetention::LiveOnly,
            message_format: self.message_format.clone(),
        }
    }
}

/// A pairing topic as written. `retention` is read only to refuse it by name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPairingTopic {
    emitted_by: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    qos_profile: QoSProfile,
    #[serde(default)]
    message_format: Option<MessageFormat>,
    #[serde(default, deserialize_with = "config::node::deserialize_present")]
    retention: Option<()>,
}

impl TryFrom<RawPairingTopic> for PairingTopic {
    type Error = String;

    fn try_from(raw: RawPairingTopic) -> Result<Self, String> {
        if raw.retention.is_some() {
            return Err(format!(
                "pairing topic `{}` must not carry `retention`: a pairing topic is live only",
                raw.name
            ));
        }
        Ok(Self {
            emitted_by: raw.emitted_by,
            name: raw.name,
            qos_profile: raw.qos_profile,
            message_format: raw.message_format,
        })
    }
}

/// Custom deserialization enforcing the pairing-document invariants after
/// the default shape parse:
///
/// - exactly two distinct role names, neither the reserved `_` sentinel;
/// - a `services` or `actions` key gets a targeted "pairings are
///   topics-only" error instead of a generic unknown-field message;
/// - a non-empty `topics` list (a pairing with no topics carries nothing);
///   a role with zero topics is fine: that is a strictly one-directional
///   pair;
/// - every `emitted_by` names one of the two declared roles;
/// - topic names are unique across the whole flat list.
impl<'de> Deserialize<'de> for PeppyPairing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawPeppyPairing {
            #[serde(deserialize_with = "deserialize_pairing_v1_schema")]
            peppy_schema: PeppySchema,
            manifest: Manifest,
            #[serde(default)]
            roles: Vec<Name>,
            #[serde(default)]
            topics: Vec<PairingTopic>,
            // Captured only to produce a targeted error: pairing documents
            // are topics-only by design (see the pairing guide's "why topics
            // only" section). A null is a present key.
            #[serde(default, deserialize_with = "config::node::deserialize_present")]
            services: Option<()>,
            #[serde(default, deserialize_with = "config::node::deserialize_present")]
            actions: Option<()>,
        }

        let raw = RawPeppyPairing::deserialize(deserializer)?;

        if raw.services.is_some() || raw.actions.is_some() {
            return Err(de::Error::custom(
                "pairing documents are topics-only: `services` and `actions` are not allowed \
                 (expose them via a regular interface instead)",
            ));
        }

        let [role_a, role_b] = raw.roles.as_slice() else {
            return Err(de::Error::custom(format!(
                "a pairing declares exactly two roles, got {} ({:?})",
                raw.roles.len(),
                raw.roles.iter().map(Name::as_str).collect::<Vec<_>>(),
            )));
        };
        if role_a == role_b {
            return Err(de::Error::custom(format!(
                "pairing roles must be distinct, got `{role_a}` twice"
            )));
        }
        for role in [role_a, role_b] {
            if role.as_str() == config::consts::DEFAULT_LINK_ID_SENTINEL {
                return Err(de::Error::custom(
                    "pairing role `_` collides with the reserved default-link_id sentinel",
                ));
            }
        }

        if raw.topics.is_empty() {
            return Err(de::Error::custom(
                "a pairing must declare at least one topic (an empty pairing carries nothing)",
            ));
        }

        for topic in &raw.topics {
            if !raw.roles.iter().any(|r| r.as_str() == topic.emitted_by) {
                return Err(de::Error::custom(format!(
                    "topic `{}` is emitted_by `{}`, which is not one of the declared roles \
                     [`{role_a}`, `{role_b}`]",
                    topic.name, topic.emitted_by,
                )));
            }
        }

        validate_named_items(raw.topics.iter().map(|t| t.name.as_str()), "pairing topic")
            .map_err(de::Error::custom)?;

        Ok(PeppyPairing {
            peppy_schema: raw.peppy_schema,
            manifest: raw.manifest,
            roles: raw.roles,
            topics: raw.topics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{SchemaType, TypeToken};

    const ARM_LINK: &str = r#"{
        peppy_schema: "pairing/v1",
        manifest: { name: "arm_link", tag: "v1" },
        roles: ["controller", "arm"],
        topics: [
            {
                emitted_by: "controller",
                name: "joint_commands",
                qos_profile: "reliable",
                message_format: { target_positions: { $type: "array", $items: "f64", $length: 3 }, max_velocity: "f64" },
            },
            {
                emitted_by: "arm",
                name: "joint_states",
                qos_profile: "sensor_data",
                message_format: { positions: { $type: "array", $items: "f64", $length: 3 }, timestamp: "time" },
            },
        ],
    }"#;

    #[test]
    fn parses_arm_link_example() {
        let parsed: PeppyPairing = serde_json5::from_str(ARM_LINK).expect("arm_link should parse");
        assert_eq!(parsed.peppy_schema, PeppySchema::PairingV1);
        assert_eq!(parsed.manifest.name.as_str(), "arm_link");
        assert_eq!(parsed.manifest.tag, "v1");
        assert_eq!(
            parsed.roles.iter().map(Name::as_str).collect::<Vec<_>>(),
            ["controller", "arm"]
        );
        assert_eq!(parsed.topics.len(), 2);

        let commands = &parsed.topics[0];
        assert_eq!(commands.emitted_by, "controller");
        assert_eq!(commands.name, "joint_commands");
        assert_eq!(commands.qos_profile, QoSProfile::Reliable);
        let mf = commands.message_format.as_ref().expect("message_format");
        assert!(matches!(
            mf.0.get("max_velocity"),
            Some(SchemaType::Type(TypeToken::F64))
        ));

        assert!(parsed.has_role("arm"));
        assert!(!parsed.has_role("observer"));
    }

    #[test]
    fn one_directional_pairing_parses() {
        // A role with zero emitted topics is legal: strictly one-directional.
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "telemetry_link", tag: "v1" },
            roles: ["sender", "receiver"],
            topics: [
                { emitted_by: "sender", name: "samples" }
            ]
        }"#;
        let parsed: PeppyPairing = serde_json5::from_str(json5).expect("should parse");
        assert!(parsed.topics.iter().all(|t| t.emitted_by == "sender"));
    }

    #[test]
    fn a_pairing_topic_refuses_retention() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "telemetry_link", tag: "v1" },
            roles: ["sender", "receiver"],
            topics: [
                { emitted_by: "sender", name: "samples", retention: { latest: 1 } }
            ]
        }"#;
        let err =
            serde_json5::from_str::<PeppyPairing>(json5).expect_err("pairing topics are live only");
        serde_json5::from_str::<PeppyPairing>(&json5.replace("{ latest: 1 }", "null"))
            .expect_err("a null is a present key");
        assert!(
            err.to_string().contains(
                "pairing topic `samples` must not carry `retention`: a pairing topic is live only"
            ),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_wrong_schema_tag() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }]
        }"#;
        let err =
            serde_json5::from_str::<PeppyPairing>(json5).expect_err("contract/v1 must be rejected");
        assert!(err.to_string().contains("pairing/v1"), "error: {err}");
    }

    /// The fields of a pairing document below its `peppy_schema` header, so
    /// each rejection case writes only the field it breaks.
    struct PairingFields {
        manifest: &'static str,
        roles: &'static str,
        topics: &'static str,
        extra: &'static str,
    }

    const VALID_PAIRING: PairingFields = PairingFields {
        manifest: r#"{ name: "x", tag: "v1" }"#,
        roles: r#"["a", "b"]"#,
        topics: r#"[{ emitted_by: "a", name: "t" }]"#,
        extra: "",
    };

    fn pairing_document(fields: &PairingFields) -> String {
        let PairingFields {
            manifest,
            roles,
            topics,
            extra,
        } = fields;
        format!(
            r#"{{
                peppy_schema: "pairing/v1",
                manifest: {manifest},
                roles: {roles},
                topics: {topics},
                {extra}
            }}"#
        )
    }

    #[test]
    fn rejects_invalid_pairing_documents() {
        serde_json5::from_str::<PeppyPairing>(&pairing_document(&VALID_PAIRING))
            .expect("the base document of the rejection cases parses");

        // (what, document fields, substrings the error must contain)
        let cases: &[(&str, PairingFields, &[&str])] = &[
            (
                "one role",
                PairingFields {
                    roles: r#"["solo"]"#,
                    topics: r#"[{ emitted_by: "solo", name: "t" }]"#,
                    ..VALID_PAIRING
                },
                &["exactly two roles"],
            ),
            (
                "three roles",
                PairingFields {
                    roles: r#"["a", "b", "c"]"#,
                    ..VALID_PAIRING
                },
                &["exactly two roles"],
            ),
            (
                "duplicate roles",
                PairingFields {
                    roles: r#"["same", "same"]"#,
                    topics: r#"[{ emitted_by: "same", name: "t" }]"#,
                    ..VALID_PAIRING
                },
                &["distinct"],
            ),
            (
                "`_` role",
                PairingFields {
                    roles: r#"["_", "b"]"#,
                    topics: r#"[{ emitted_by: "b", name: "t" }]"#,
                    ..VALID_PAIRING
                },
                &["sentinel"],
            ),
            (
                // The error names the bad role and the declared roles.
                "undeclared emitted_by",
                PairingFields {
                    topics: r#"[{ emitted_by: "ghost", name: "t" }]"#,
                    ..VALID_PAIRING
                },
                &["ghost", "`a`", "`b`"],
            ),
            (
                "empty topics",
                PairingFields {
                    topics: "[]",
                    ..VALID_PAIRING
                },
                &["at least one topic"],
            ),
            (
                // The flat-list uniqueness rule: one namespace regardless of
                // which role emits, which keeps the generated
                // pairings.<link_id>.* module namespace unambiguous.
                "cross-direction duplicate topic",
                PairingFields {
                    topics: r#"[
                        { emitted_by: "a", name: "shared" },
                        { emitted_by: "b", name: "shared" }
                    ]"#,
                    ..VALID_PAIRING
                },
                &["duplicate"],
            ),
            (
                "services key",
                PairingFields {
                    extra: "services: []",
                    ..VALID_PAIRING
                },
                &["topics-only"],
            ),
            (
                "actions key",
                PairingFields {
                    extra: "actions: []",
                    ..VALID_PAIRING
                },
                &["topics-only"],
            ),
            (
                "null services key",
                PairingFields {
                    extra: "services: null",
                    ..VALID_PAIRING
                },
                &["topics-only"],
            ),
            (
                "null actions key",
                PairingFields {
                    extra: "actions: null",
                    ..VALID_PAIRING
                },
                &["topics-only"],
            ),
            (
                "unknown top-level field",
                PairingFields {
                    extra: r#"execution: { language: "rust" }"#,
                    ..VALID_PAIRING
                },
                &[],
            ),
            (
                // The shared contract/pairing doc Manifest is
                // deny_unknown_fields and has no depends_on: a pairing is a
                // passive contract.
                "manifest depends_on",
                PairingFields {
                    manifest: r#"{ name: "x", tag: "v1", depends_on: { nodes: [] } }"#,
                    ..VALID_PAIRING
                },
                &[],
            ),
            (
                "empty topic name",
                PairingFields {
                    topics: r#"[{ emitted_by: "a" }]"#,
                    ..VALID_PAIRING
                },
                &["empty"],
            ),
        ];

        for (what, fields, expected) in cases {
            let err = serde_json5::from_str::<PeppyPairing>(&pairing_document(fields))
                .expect_err(&format!("{what}: the document must be rejected"));
            let message = err.to_string();
            for substring in *expected {
                assert!(
                    message.contains(substring),
                    "{what}: the error should contain {substring:?}: {message}"
                );
            }
        }
    }

    #[test]
    fn round_trips_through_serde() {
        let original: PeppyPairing = serde_json5::from_str(ARM_LINK).expect("parse");
        let serialized = serde_json5::to_string(&original).expect("serialize");
        let reparsed: PeppyPairing = serde_json5::from_str(&serialized).expect("re-parse");
        assert_eq!(original, reparsed);
    }
}
