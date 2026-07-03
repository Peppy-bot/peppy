use crate::internal::interface::{Manifest, validate_named_items};
use config::{
    node::{MessageFormat, QoSProfile},
    runtime::Name,
    schema::PeppySchema,
};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};

/// Reject any `peppy_schema` value other than `pairing/v1` so a node,
/// launcher, or interface document can't slip through `PeppyPairingParser`.
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
/// role's name. There is no nesting and no convention — the flat `topics`
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
    /// The counterpart of `role` in this pairing, or `None` when `role` is
    /// not one of the document's two roles.
    pub fn counterpart_role(&self, role: &str) -> Option<&str> {
        let [a, b] = self.roles.as_slice() else {
            return None;
        };
        if a.as_str() == role {
            Some(b.as_str())
        } else if b.as_str() == role {
            Some(a.as_str())
        } else {
            None
        }
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.as_str() == role)
    }

    /// Topics the given role emits (the other role consumes them).
    pub fn topics_emitted_by<'a>(
        &'a self,
        role: &'a str,
    ) -> impl Iterator<Item = &'a PairingTopic> {
        self.topics.iter().filter(move |t| t.emitted_by == role)
    }
}

/// One topic of the conversation. The [`config::node::EmittedTopic`] fields
/// plus `emitted_by`, which names the emitting role; the other role consumes
/// the topic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PairingTopic {
    pub emitted_by: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
}

/// Custom deserialization enforcing the pairing-document invariants after
/// the default shape parse:
///
/// - exactly two distinct role names, neither the reserved `_` sentinel;
/// - a `services` or `actions` key gets a targeted "pairings are
///   topics-only" error instead of a generic unknown-field message;
/// - a non-empty `topics` list (a pairing with no topics carries nothing);
///   a role with zero topics is fine — that is a strictly one-directional
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
            // only" section).
            #[serde(default)]
            services: Option<serde_json::Value>,
            #[serde(default)]
            actions: Option<serde_json::Value>,
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

        assert_eq!(parsed.counterpart_role("controller"), Some("arm"));
        assert_eq!(parsed.counterpart_role("arm"), Some("controller"));
        assert_eq!(parsed.counterpart_role("observer"), None);
        assert!(parsed.has_role("arm"));
        assert!(!parsed.has_role("observer"));
        assert_eq!(parsed.topics_emitted_by("arm").count(), 1);
        assert_eq!(
            parsed
                .topics_emitted_by("controller")
                .next()
                .map(|t| t.name.as_str()),
            Some("joint_commands")
        );
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
        assert_eq!(parsed.topics_emitted_by("receiver").count(), 0);
    }

    #[test]
    fn rejects_wrong_schema_tag() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5)
            .expect_err("interface/v1 must be rejected");
        assert!(err.to_string().contains("pairing/v1"), "error: {err}");
    }

    #[test]
    fn rejects_one_role() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["solo"],
            topics: [{ emitted_by: "solo", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("one role rejected");
        assert!(
            err.to_string().contains("exactly two roles"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_three_roles() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b", "c"],
            topics: [{ emitted_by: "a", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("three roles rejected");
        assert!(
            err.to_string().contains("exactly two roles"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_roles() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["same", "same"],
            topics: [{ emitted_by: "same", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("dup roles rejected");
        assert!(err.to_string().contains("distinct"), "error: {err}");
    }

    #[test]
    fn rejects_sentinel_role() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["_", "b"],
            topics: [{ emitted_by: "b", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("`_` role rejected");
        assert!(err.to_string().contains("sentinel"), "error: {err}");
    }

    #[test]
    fn rejects_emitted_by_undeclared_role() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "ghost", name: "t" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5)
            .expect_err("undeclared emitted_by rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("ghost") && msg.contains("`a`") && msg.contains("`b`"),
            "error should name the bad role and the declared roles: {msg}"
        );
    }

    #[test]
    fn rejects_empty_topics() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: []
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("empty topics rejected");
        assert!(
            err.to_string().contains("at least one topic"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_topic_names_across_directions() {
        // The flat-list uniqueness rule: one namespace regardless of which
        // role emits — this keeps the generated pairings.<link_id>.* module
        // namespace unambiguous.
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [
                { emitted_by: "a", name: "shared" },
                { emitted_by: "b", name: "shared" }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5)
            .expect_err("cross-direction duplicate rejected");
        assert!(err.to_string().contains("duplicate"), "error: {err}");
    }

    #[test]
    fn rejects_services_key_with_topics_only_error() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }],
            services: []
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("services rejected");
        assert!(err.to_string().contains("topics-only"), "error: {err}");
    }

    #[test]
    fn rejects_actions_key_with_topics_only_error() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }],
            actions: []
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("actions rejected");
        assert!(err.to_string().contains("topics-only"), "error: {err}");
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }],
            execution: { language: "rust" }
        }"#;
        assert!(serde_json5::from_str::<PeppyPairing>(json5).is_err());
    }

    #[test]
    fn rejects_manifest_depends_on() {
        // The shared interface/pairing doc Manifest is deny_unknown_fields
        // and has no depends_on: a pairing is a passive contract.
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1", depends_on: { nodes: [] } },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "t" }]
        }"#;
        assert!(serde_json5::from_str::<PeppyPairing>(json5).is_err());
    }

    #[test]
    fn rejects_empty_topic_name() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "x", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a" }]
        }"#;
        let err = serde_json5::from_str::<PeppyPairing>(json5).expect_err("empty name rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    #[test]
    fn round_trips_through_serde() {
        let original: PeppyPairing = serde_json5::from_str(ARM_LINK).expect("parse");
        let serialized = serde_json5::to_string(&original).expect("serialize");
        let reparsed: PeppyPairing = serde_json5::from_str(&serialized).expect("re-parse");
        assert_eq!(original, reparsed);
    }
}
