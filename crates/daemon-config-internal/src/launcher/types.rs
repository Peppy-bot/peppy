use crate::error::StructuredError;
use crate::internal::contract::validate_named_items;
use config::{AnyType, consts::DEFAULT_LINK_ID_SENTINEL, runtime::Name, schema::PeppySchema};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::collections::{BTreeMap, HashSet};

pub use crate::source::{
    DeploymentGitSource, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
    DeploymentUrlSource,
};

#[derive(Debug, Clone, Serialize)]
pub struct PeppyLauncher {
    pub peppy_schema: PeppySchema,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<Deployment>,
}

/// Custom deserialization for [`PeppyLauncher`] that, after the default
/// shape parse, cross-checks every `bindings` value against the
/// set of `instance_id`s declared across all deployments. A binding that
/// points at an unknown instance is rejected with a structured
/// [`StructuredError::UnknownInstanceId`] so callers see a path-aware
/// message instead of a generic serde error.
impl<'de> Deserialize<'de> for PeppyLauncher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawPeppyLauncher {
            #[serde(deserialize_with = "deserialize_launcher_v1_schema")]
            peppy_schema: PeppySchema,
            #[serde(default)]
            deployments: Vec<Deployment>,
        }

        let raw = RawPeppyLauncher::deserialize(deserializer)?;

        let known_ids: HashSet<&str> = raw
            .deployments
            .iter()
            .flat_map(|d| d.instances.iter())
            .map(|i| i.instance_id.as_str())
            .collect();

        for deployment in &raw.deployments {
            for instance in &deployment.instances {
                for (binding, value) in &instance.bindings {
                    if binding == DEFAULT_LINK_ID_SENTINEL {
                        let err = StructuredError::BindingSentinelKey {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding.clone(),
                        };
                        return Err(de::Error::custom(err.json5_message()));
                    }
                    for target in value.targets() {
                        if !known_ids.contains(target.as_str()) {
                            let err = StructuredError::UnknownInstanceId {
                                owner_instance_id: instance.instance_id.to_string(),
                                binding: binding.clone(),
                                instance_id: target.clone(),
                            };
                            return Err(de::Error::custom(err.json5_message()));
                        }
                    }
                }
                for (key, target) in &instance.pairings {
                    if key == DEFAULT_LINK_ID_SENTINEL {
                        let err = StructuredError::PairingSentinelKey {
                            owner_instance_id: instance.instance_id.to_string(),
                            key: key.clone(),
                        };
                        return Err(de::Error::custom(err.json5_message()));
                    }
                    let (target_instance, _peer_link) = split_pair_target(target);
                    if !known_ids.contains(target_instance) {
                        let err = StructuredError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: key.clone(),
                            instance_id: target_instance.to_string(),
                        };
                        return Err(de::Error::custom(err.json5_message()));
                    }
                }
            }
        }

        Ok(PeppyLauncher {
            peppy_schema: raw.peppy_schema,
            deployments: raw.deployments,
        })
    }
}

/// Reject any `peppy_schema` value other than `launcher/v1` so a node
/// document that happens to share the launcher's deployment shape can't
/// slip through `PeppyLauncherParser`.
fn deserialize_launcher_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::LauncherV1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub source: DeploymentSource,
    #[serde(deserialize_with = "deserialize_instances")]
    pub instances: Vec<DeploymentInstance>,
}

impl DeploymentInstance {
    /// An instance entry carrying only its id, every other field at its
    /// default-empty value. Validation feeders use this to represent
    /// already-running instances without fabricating per-instance data the
    /// validators do not consult.
    pub fn empty(instance_id: Name) -> Self {
        Self {
            instance_id,
            arguments: BTreeMap::new(),
            env_vars: BTreeMap::new(),
            framework: FrameworkOverrides::default(),
            bindings: BTreeMap::new(),
            pairings: BTreeMap::new(),
            defer_pairings: Vec::new(),
        }
    }
}

/// One `bindings:` value: the producer target(s) selected for a declared
/// slot, remembering the shape they arrived in. A binding value's shape
/// mirrors the slot's declared cardinality, but the launch parser has no
/// manifest knowledge, so both launch-file shapes parse everywhere and
/// `validate_bindings` enforces shape-vs-cardinality at plan time. Shape-
/// local rules (empty-string targets, duplicate targets within one slot)
/// still fail at parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingValue {
    /// `camera: "front_camera"` — the launch-file scalar shape, valid only
    /// on a `cardinality: "one"` slot.
    Scalar(String),
    /// `camera: ["front_camera", "rear_camera"]` — the launch-file array
    /// shape, valid only on `one_or_more` / `zero_or_more` slots (where
    /// `[]` is a valid definition for `zero_or_more`).
    Array(Vec<String>),
    /// Accumulated `--bind camera@front --bind camera@rear` occurrences in
    /// flag order. Flag repetition carries no scalar/array shape, so the
    /// validator checks it against the slot's cardinality by count alone.
    /// Built by the CLI; never parsed from a launch file. Non-empty by
    /// construction (zero occurrences is an omitted binding).
    Flags(Vec<String>),
}

impl BindingValue {
    /// The target instance ids in declaration order, shape-erased.
    pub fn targets(&self) -> &[String] {
        match self {
            BindingValue::Scalar(target) => std::slice::from_ref(target),
            BindingValue::Array(targets) | BindingValue::Flags(targets) => targets,
        }
    }
}

/// Serializes back to the launch-file shapes: `Scalar` as a string,
/// `Array` as an array. `Flags` also serializes as an array — it exists
/// only on CLI-built plans, which are never round-tripped through a launch
/// file, and the array form is its closest document equivalent.
impl Serialize for BindingValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            BindingValue::Scalar(target) => serializer.serialize_str(target),
            BindingValue::Array(targets) | BindingValue::Flags(targets) => {
                targets.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for BindingValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BindingValueVisitor;

        impl<'de> de::Visitor<'de> for BindingValueVisitor {
            type Value = BindingValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str("a producer instance_id string or an array of instance_id strings")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(BindingValue::Scalar(v.to_string()))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut targets: Vec<String> = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(target) = seq.next_element::<String>()? {
                    targets.push(target);
                }
                Ok(BindingValue::Array(targets))
            }
        }

        deserializer.deserialize_any(BindingValueVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInstance {
    pub instance_id: Name,
    #[serde(default)]
    pub arguments: BTreeMap<String, AnyType>,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
    #[serde(default)]
    pub framework: FrameworkOverrides,
    #[serde(
        default,
        deserialize_with = "deserialize_bindings",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub bindings: BTreeMap<String, BindingValue>,
    /// Pairing declarations: own pairing-slot `link_id` → peer instance
    /// (`"<instance_id>"` or `"<instance_id>/<peer_link_id>"` when the peer
    /// has more than one complementary slot). Declaring the pair on ONE side
    /// covers both endpoints' slots; declaring it from both sides is allowed
    /// but must agree. Mirror of `bindings` for the pairing mechanism.
    #[serde(
        default,
        deserialize_with = "deserialize_pairings",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub pairings: BTreeMap<String, String>,
    /// Required pairing slots deliberately left unpaired at launch. Every
    /// required slot must be paired or listed here, or the launch fails
    /// loudly (`PairingSlotUncovered`). Optional slots need no entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defer_pairings: Vec<String>,
}

/// Each key is a `link_id` literal declared by the deployed node's
/// `depends_on.{nodes,contracts}` and each value selects the slot's
/// producer target(s): a scalar `instance_id` string or an array of them
/// (see [`BindingValue`]). Both shapes parse here because the launch
/// parser has no manifest knowledge; whether the shape matches the slot's
/// declared cardinality is enforced in `validate_bindings` at plan time.
/// Shape-local rules fail at parse: keys are validated for non-emptiness
/// and intra-collection duplicates via [`validate_named_items`], targets
/// must be non-empty strings, and a target may appear at most once within
/// one slot's array. The reserved producer-default sentinel
/// ([`DEFAULT_LINK_ID_SENTINEL`]) is rejected as a key at the
/// [`PeppyLauncher`] level, as is each target's existence as an
/// `instance_id` once all deployments have been parsed.
fn deserialize_bindings<'de, D>(deserializer: D) -> Result<BTreeMap<String, BindingValue>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries =
        deserializer.deserialize_map(BindingEntriesVisitor::<BindingValue>::new("binding"))?;
    validate_named_items(entries.iter().map(|(k, _)| k.as_str()), "binding")
        .map_err(de::Error::custom)?;
    let mut out = BTreeMap::new();
    for (key, value) in entries {
        for (idx, target) in value.targets().iter().enumerate() {
            if target.trim().is_empty() {
                return Err(de::Error::custom(format!(
                    "binding target for key `{key}` cannot be empty"
                )));
            }
            if value.targets()[..idx].contains(target) {
                return Err(de::Error::custom(format!(
                    "binding `{key}` names target `{target}` more than once: a slot's \
                     bound set lists each producer once"
                )));
            }
        }
        out.insert(key, value);
    }
    Ok(out)
}

/// Mirror of [`deserialize_bindings`] for the per-instance `pairings` map:
/// keys are the instance's own pairing-slot link_ids, values name the peer
/// instance (optionally suffixed `/<peer_link_id>`). Duplicate keys, empty
/// keys/values, and malformed targets are rejected here; sentinel keys and
/// unknown target instances are checked at the [`PeppyLauncher`] level where
/// the owning `instance_id` and the full instance set are in scope.
/// Entries are captured as a Vec because a direct `BTreeMap::deserialize`
/// would silently overwrite duplicate keys, hiding them from
/// `validate_named_items`. Duplicate VALUES are intentionally permitted:
/// one peer may serve multiple `link_id` slots.
fn deserialize_pairings<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries = deserializer.deserialize_map(BindingEntriesVisitor::<String>::new("pairing"))?;
    validate_named_items(entries.iter().map(|(k, _)| k.as_str()), "pairing")
        .map_err(de::Error::custom)?;
    for (key, value) in &entries {
        if value.trim().is_empty() {
            return Err(de::Error::custom(format!(
                "pairing target for key `{key}` cannot be empty"
            )));
        }
        // Reject malformed targets at parse time rather than letting an
        // empty or slash-bearing peer_link fail later as "no complementary
        // slot" during plan validation.
        let (instance, peer_link) = split_pair_target(value);
        if instance.is_empty() || peer_link.is_some_and(|l| l.is_empty() || l.contains('/')) {
            return Err(de::Error::custom(format!(
                "pairing target `{value}` for key `{key}` is malformed: expected \
                 `<peer_instance>` or `<peer_instance>/<peer_link_id>`"
            )));
        }
    }
    Ok(entries.into_iter().collect())
}

/// Splits a launcher `pairings` value (or CLI `--pair` right-hand side) into
/// `(peer_instance_id, Option<peer_link_id>)`. The `/` separator cannot
/// appear inside wire segments, so the split is unambiguous.
pub fn split_pair_target(value: &str) -> (&str, Option<&str>) {
    match value.split_once('/') {
        Some((instance, peer_link)) => (instance, Some(peer_link)),
        None => (value, None),
    }
}

/// Map visitor shared by the binding and pairing deserializers, generic
/// over the value shape (a single target string for pairings, one-or-many
/// targets for bindings). Collects into a Vec so duplicate keys survive to
/// `validate_named_items` instead of being collapsed by a map insert.
struct BindingEntriesVisitor<V> {
    /// Entry kind used to prefix value errors with the owning key
    /// (`"binding"` / `"pairing"`), so e.g. an array binding value fails
    /// as "binding `uvc_camera`: …" instead of a bare type error.
    label: &'static str,
    _value: std::marker::PhantomData<V>,
}

impl<V> BindingEntriesVisitor<V> {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            _value: std::marker::PhantomData,
        }
    }
}

impl<'de, V: Deserialize<'de>> Visitor<'de> for BindingEntriesVisitor<V> {
    type Value = Vec<(String, V)>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a map of link_id -> producer instance_id target")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(key) = access.next_key::<String>()? {
            let value = access
                .next_value::<V>()
                .map_err(|err| de::Error::custom(format!("{} `{key}`: {err}", self.label)))?;
            entries.push((key, value));
        }
        Ok(entries)
    }
}

/// Per-instance framework knobs. Distinct from `arguments`: those are
/// declared by the node author and validated against a per-node parameter
/// schema; framework knobs are owned by peppylib, fixed-shape, and applied
/// uniformly to every node. Each field is optional so the daemon can fall
/// through to its own default when the instance omits the override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_sim_time: Option<bool>,
}

fn deserialize_instances<'de, D>(deserializer: D) -> Result<Vec<DeploymentInstance>, D::Error>
where
    D: Deserializer<'de>,
{
    let instances = Vec::<DeploymentInstance>::deserialize(deserializer)?;
    let mut seen = HashSet::with_capacity(instances.len());
    for instance in &instances {
        let id = instance.instance_id.to_string();
        if !seen.insert(id.clone()) {
            let err = crate::error::StructuredError::DuplicateName(id);
            return Err(de::Error::custom(err.json5_message()));
        }
    }
    Ok(instances)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ParsingError;

    #[test]
    fn duplicate_instance_ids_are_rejected() {
        let duplicate_instances = r#"{
            source: { local: "./uvc_camera" },
            instances: [
                { instance_id: "camera_front" },
                { instance_id: "camera_front" }
            ]
        }"#;

        let err = serde_json5::from_str::<Deployment>(duplicate_instances)
            .expect_err("expected duplicate instance_id rejection");
        let ParsingError::DuplicateName(duplicate) = ParsingError::from(err) else {
            panic!("expected duplicate instance id error");
        };
        assert_eq!(duplicate, "camera_front");
    }

    /// Verifies that optional fields (`arguments`, `env_vars`, `framework`)
    /// default to empty when omitted, and that partially specified instances
    /// deserialize correctly.
    #[test]
    fn deployment_instance_defaults() {
        let instance: DeploymentInstance =
            serde_json5::from_str("{ instance_id: \"camera_front\" }").unwrap();
        assert_eq!(instance.instance_id, "camera_front");
        assert!(instance.arguments.is_empty());
        assert!(instance.env_vars.is_empty());
        assert_eq!(instance.framework.use_sim_time, None);

        let with_env: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"esp32_1\", env_vars: { ESP32_DEVICE: \"/dev/ttyUSB0\" } }",
        )
        .unwrap();
        assert_eq!(with_env.instance_id, "esp32_1");
        assert_eq!(
            with_env.env_vars.get("ESP32_DEVICE").map(String::as_str),
            Some("/dev/ttyUSB0")
        );
    }

    /// Per-instance framework overrides parse cleanly and round-trip back
    /// to JSON5. Both the explicit-true and explicit-false cases must be
    /// distinguishable from "absent" so the daemon's precedence (per-instance
    /// > daemon CLI flag > default) has a value to gate on.
    #[test]
    fn deployment_instance_framework_overrides_round_trip() {
        let with_sim: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"camera_front\", framework: { use_sim_time: true } }",
        )
        .unwrap();
        assert_eq!(with_sim.framework.use_sim_time, Some(true));

        let with_wall: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"camera_front\", framework: { use_sim_time: false } }",
        )
        .unwrap();
        assert_eq!(with_wall.framework.use_sim_time, Some(false));

        let serialized = serde_json5::to_string(&with_sim).unwrap();
        let reparsed: DeploymentInstance = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.framework.use_sim_time, Some(true));
    }

    /// A binding's value pointing at an `instance_id` defined in a sibling
    /// deployment must resolve; the bindings on the consumer instance
    /// round-trip with the exact keys/values that were written.
    #[test]
    fn bindings_resolve_against_siblings() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { local: "./left" },
                    instances: [{ instance_id: "cam_wrist_left", arguments: {} }]
                },
                {
                    source: { local: "./right" },
                    instances: [{ instance_id: "cam_wrist_right", arguments: {} }]
                },
                {
                    source: { local: "./torso" },
                    instances: [{ instance_id: "cam_torso", arguments: {} }]
                },
                {
                    source: { local: "./backbone" },
                    instances: [{
                        instance_id: "backbone",
                        bindings: {
                            wrist_left_camera: "cam_wrist_left",
                            wrist_right_camera: "cam_wrist_right",
                            torso_camera: "cam_torso",
                        }
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher = serde_json5::from_str(json5).expect("launcher should parse");
        let backbone = &launcher.deployments[3].instances[0];
        assert_eq!(backbone.instance_id, "backbone");
        assert_eq!(backbone.bindings.len(), 3);
        assert_eq!(
            backbone.bindings.get("torso_camera"),
            Some(&BindingValue::Scalar("cam_torso".to_string()))
        );
    }

    /// Both launch-file shapes parse everywhere (the parser has no manifest
    /// knowledge): a string parses as `Scalar`, an array of any length
    /// (empty included, the valid `zero_or_more` empty set) as `Array`,
    /// with declaration order preserved. Whether the shape matches the
    /// slot's cardinality is enforced later in `validate_bindings`.
    #[test]
    fn bindings_parse_scalar_and_array_shapes() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: {
                main: "camera_inst",
                arm_states: ["right_arm_inst", "left_arm_inst"],
                spare_cameras: []
            }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("both shapes should parse");
        assert_eq!(
            instance.bindings.get("main"),
            Some(&BindingValue::Scalar("camera_inst".to_string()))
        );
        assert_eq!(
            instance.bindings.get("arm_states"),
            Some(&BindingValue::Array(vec![
                "right_arm_inst".to_string(),
                "left_arm_inst".to_string()
            ])),
            "array order must be preserved, not sorted"
        );
        assert_eq!(
            instance.bindings.get("spare_cameras"),
            Some(&BindingValue::Array(Vec::new()))
        );
    }

    /// Shape-local parse rule: the same target twice within one slot's
    /// array is rejected at parse, naming the slot and the target.
    #[test]
    fn bindings_reject_duplicate_targets_within_one_slot() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: { arm_states: ["arm_inst", "other_inst", "arm_inst"] }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("duplicate target within one slot must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("`arm_states`") && msg.contains("`arm_inst`"),
            "error must name the slot and the duplicated target: {msg}"
        );
        assert!(
            msg.contains("once"),
            "error must state the each-producer-once rule: {msg}"
        );
    }

    /// Shape-local parse rule: empty-string targets are rejected inside
    /// arrays exactly as they are for scalars.
    #[test]
    fn bindings_reject_empty_target_inside_array() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: { arm_states: ["arm_inst", ""] }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty target inside an array must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn bindings_default_to_empty_when_omitted() {
        let instance: DeploymentInstance =
            serde_json5::from_str("{ instance_id: \"camera_front\" }").unwrap();
        assert!(instance.bindings.is_empty());
    }

    /// A binding value that doesn't match any `instance_id` declared across
    /// the launcher must surface as a structured `UnknownInstanceId` error,
    /// not a generic serde message.
    #[test]
    fn bindings_reject_unknown_instance_id() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { local: "./backbone" },
                    instances: [{
                        instance_id: "backbone",
                        bindings: {
                            torso_camera: "does_not_exist"
                        }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("unknown instance_id must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            binding,
            instance_id,
        } = parsing_err
        else {
            panic!("expected UnknownInstanceId, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "backbone");
        assert_eq!(binding, "torso_camera");
        assert_eq!(instance_id, "does_not_exist");
    }

    #[test]
    fn bindings_reject_empty_key() {
        let json5 = r#"{
            instance_id: "backbone",
            bindings: { "": "cam_torso" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty binding key must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn bindings_reject_empty_value() {
        let json5 = r#"{
            instance_id: "backbone",
            bindings: { torso_camera: "" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty binding value must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    /// Two binding keys may point at the same producer `instance_id`:
    /// that is the "one producer serves multiple `link_id` slots" case
    /// the wiring step materializes as a producer with multiple
    /// concurrent `link_ids` on the wire. Duplicates on the value side
    /// are therefore intentionally permitted.
    #[test]
    fn bindings_accept_duplicate_values() {
        let json5 = r#"{
            instance_id: "backbone",
            bindings: {
                a: "cam_torso",
                b: "cam_torso"
            }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("duplicate binding targets should now be accepted");
        assert_eq!(
            instance.bindings.get("a"),
            Some(&BindingValue::Scalar("cam_torso".to_string()))
        );
        assert_eq!(
            instance.bindings.get("b"),
            Some(&BindingValue::Scalar("cam_torso".to_string()))
        );
    }

    /// The launcher rejects unknown framework keys so a typo (e.g.
    /// `use_simulation_time`) does not silently fall through to wall mode.
    #[test]
    fn deployment_instance_framework_rejects_unknown_keys() {
        let err = serde_json5::from_str::<DeploymentInstance>(
            "{ instance_id: \"camera_front\", framework: { unknown_knob: true } }",
        )
        .expect_err("unknown framework key should be rejected");
        assert!(err.to_string().contains("unknown_knob"));
    }

    /// The reserved producer-default segment cannot appear as a binding
    /// key. Using it would be a redundant no-op (the producer already
    /// publishes under that segment when no binding is declared) and
    /// likely indicates a misuse. The check runs at the launcher level
    /// (rather than per-instance) so the structured error can carry the
    /// owning `instance_id`.
    #[test]
    fn bindings_reject_underscore_key() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { local: "./backbone" },
                    instances: [{
                        instance_id: "backbone",
                        bindings: { "_": "backbone" }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("`_` binding key must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::BindingSentinelKey {
            owner_instance_id,
            binding,
        } = &parsing_err
        else {
            panic!("expected BindingSentinelKey, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "backbone");
        assert_eq!(binding, "_");
    }

    /// The `pairings` map parses, resolves against siblings, and supports
    /// the `/<peer_link_id>` disambiguation suffix; `defer_pairings` rides
    /// alongside.
    #[test]
    fn pairings_and_defer_pairings_parse() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [{ instance_id: "arm_1" }]
                },
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        pairings: { arm: "arm_1" },
                        defer_pairings: ["spare"]
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher = serde_json5::from_str(json5).expect("launcher should parse");
        let ctrl = &launcher.deployments[1].instances[0];
        assert_eq!(ctrl.pairings.get("arm").map(String::as_str), Some("arm_1"));
        assert_eq!(ctrl.defer_pairings, vec!["spare".to_string()]);

        // The peer-slot suffix parses and still resolves the instance part.
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [{ instance_id: "arm_1" }]
                },
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        pairings: { arm: "arm_1/controller" }
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher =
            serde_json5::from_str(json5).expect("suffixed pairing should parse");
        let ctrl = &launcher.deployments[1].instances[0];
        assert_eq!(
            ctrl.pairings.get("arm").map(String::as_str),
            Some("arm_1/controller")
        );
        assert_eq!(
            split_pair_target("arm_1/controller"),
            ("arm_1", Some("controller"))
        );
        assert_eq!(split_pair_target("arm_1"), ("arm_1", None));
    }

    /// A pairing value naming an unknown instance is a structured error,
    /// even with the `/<peer_link_id>` suffix (only the instance part is
    /// resolved).
    #[test]
    fn pairings_reject_unknown_instance_id() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        pairings: { arm: "ghost/controller" }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("unknown pairing target must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            binding,
            instance_id,
        } = parsing_err
        else {
            panic!("expected UnknownInstanceId, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "ctrl_1");
        assert_eq!(binding, "arm");
        assert_eq!(instance_id, "ghost");
    }

    /// The reserved `_` sentinel cannot be a pairing key, mirroring the
    /// binding-key rule.
    #[test]
    fn pairings_reject_underscore_key() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        pairings: { "_": "ctrl_1" }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("`_` pairing key must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::PairingSentinelKey {
            owner_instance_id,
            key,
        } = &parsing_err
        else {
            panic!("expected PairingSentinelKey, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "ctrl_1");
        assert_eq!(key, "_");
        assert!(
            parsing_err.to_string().contains("pairing"),
            "message should use pairing wording: {parsing_err}"
        );
    }

    #[test]
    fn pairings_reject_duplicate_and_empty_entries() {
        let dup = r#"{
            instance_id: "ctrl_1",
            pairings: { "arm": "a1", "arm": "a2" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(dup)
            .expect_err("duplicate pairing key must be rejected");
        assert!(err.to_string().contains("duplicate"), "error: {err}");

        let empty_value = r#"{
            instance_id: "ctrl_1",
            pairings: { "arm": "" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(empty_value)
            .expect_err("empty pairing target must be rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");

        let empty_key = r#"{
            instance_id: "ctrl_1",
            pairings: { "": "arm_1" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(empty_key)
            .expect_err("empty pairing key must be rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    /// A pairing target must be `<peer_instance>` or
    /// `<peer_instance>/<peer_link_id>`; empty parts or extra `/` segments
    /// fail at parse time with a targeted message instead of surfacing later
    /// as a plan-phase "no complementary slot" error.
    #[test]
    fn pairings_reject_malformed_targets() {
        for target in ["arm_1/", "/controller", "arm_1/ctl/extra"] {
            let json5 = format!(
                r#"{{
                    instance_id: "ctrl_1",
                    pairings: {{ "arm": "{target}" }}
                }}"#
            );
            let err = serde_json5::from_str::<DeploymentInstance>(&json5)
                .expect_err("malformed pairing target must be rejected");
            assert!(
                err.to_string().contains("malformed"),
                "target `{target}` should be reported as malformed: {err}"
            );
        }
    }

    /// Duplicate binding keys must be rejected. The raw map deserializer
    /// must surface them before the BTreeMap collapses duplicates.
    #[test]
    fn bindings_reject_duplicate_keys() {
        let json5 = r#"{
            instance_id: "backbone",
            bindings: { "main": "prod_a", "main": "prod_b" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("duplicate binding key must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate") && msg.contains("main"),
            "unexpected error: {msg}"
        );
    }
}
