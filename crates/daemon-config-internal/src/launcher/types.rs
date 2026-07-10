use crate::error::StructuredError;
use crate::internal::interface::validate_named_items;
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
                for (binding, targets) in &instance.bindings {
                    if binding == DEFAULT_LINK_ID_SENTINEL {
                        let err = StructuredError::BindingSentinelKey {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding.clone(),
                        };
                        return Err(de::Error::custom(err.json5_message()));
                    }
                    for target in targets.targets() {
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
    /// Consumer-slot bindings: the deployed node's `depends_on` `link_id`
    /// → one producer `instance_id` (pinned slots) or an array of them
    /// (from_any slots). An omitted key or an empty array leaves a
    /// from_any slot deliberately unbound (silent). See
    /// [`BindingTargets`].
    #[serde(
        default,
        deserialize_with = "deserialize_bindings",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub bindings: BTreeMap<String, BindingTargets>,
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

/// One binding's producer set, as written in the launcher: a scalar
/// `"instance_id"` (the single-producer form) or an array `["a", "b"]`
/// (a from_any slot bound to several producers). An **empty array is
/// legal** and means the same as omitting the key — the slot stays
/// deliberately unbound (silent). Elements must be non-empty and unique;
/// declaration order is preserved. Serializes back to the scalar form
/// when it holds exactly one target, so a parse → serialize round-trip
/// is byte-stable for scalar-form launchers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingTargets(Vec<String>);

impl BindingTargets {
    /// Validated constructor applying the same rules as the
    /// deserializer: no empty-string elements, no duplicate elements.
    /// Empty vectors are accepted (deliberately unbound).
    pub fn new(targets: Vec<String>) -> Result<Self, String> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(targets.len());
        for target in &targets {
            if target.trim().is_empty() {
                return Err("binding target cannot be empty".to_string());
            }
            if !seen.insert(target.as_str()) {
                return Err(format!(
                    "duplicate binding target `{target}` (each producer may be listed once)"
                ));
            }
        }
        Ok(Self(targets))
    }

    /// The bound producer `instance_id`s, in declaration order. Empty
    /// means the slot is deliberately unbound.
    pub fn targets(&self) -> &[String] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for BindingTargets {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0.as_slice() {
            // Scalar round-trip: a single target re-serializes to the
            // scalar form so rewriting a launcher is byte-stable.
            [single] => serializer.serialize_str(single),
            targets => targets.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for BindingTargets {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TargetsVisitor;

        impl<'de> Visitor<'de> for TargetsVisitor {
            type Value = BindingTargets;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a producer instance_id string or an array of producer instance_id strings",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                BindingTargets::new(vec![value.to_string()]).map_err(E::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut targets = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(target) = seq.next_element::<String>()? {
                    targets.push(target);
                }
                BindingTargets::new(targets).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(TargetsVisitor)
    }
}

/// Each key is a `link_id` literal declared by the deployed node's
/// `depends_on.{nodes,interfaces}` and each value names the producer
/// `instance_id`(s) defined elsewhere in the launcher — one for a pinned
/// slot, zero or more for a `from_any` slot (see [`BindingTargets`]).
/// Keys are validated for non-emptiness and intra-collection duplicates
/// via [`validate_named_items`]; per-element value rules live in
/// [`BindingTargets`]; the reserved producer-default sentinel
/// ([`DEFAULT_LINK_ID_SENTINEL`]) is rejected as a key at the
/// [`PeppyLauncher`] level. Each element's existence as an `instance_id`
/// is checked there too, once all deployments have been parsed; the
/// key's existence in the deployed node's `depends_on` and the producer
/// identity are checked at launch time, when both the launcher and the
/// node manifests are loaded.
fn deserialize_bindings<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, BindingTargets>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(deserialize_kv_entries(deserializer, "binding")?
        .into_iter()
        .collect())
}

/// Mirror of [`deserialize_bindings`] for the per-instance `pairings` map:
/// keys are the instance's own pairing-slot link_ids, values name the peer
/// instance (optionally suffixed `/<peer_link_id>`). Duplicate keys, empty
/// keys/values, and malformed targets are rejected here; sentinel keys and
/// unknown target instances are checked at the [`PeppyLauncher`] level where
/// the owning `instance_id` and the full instance set are in scope.
fn deserialize_pairings<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries: Vec<(String, String)> = deserialize_kv_entries(deserializer, "pairing")?;
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

/// Shared scaffold of [`deserialize_bindings`] / [`deserialize_pairings`]:
/// duplicate-key and empty-key checks over a `link_id -> V` map, with
/// `kind` labeling the errors. Entries are captured as a Vec because a
/// direct `BTreeMap::deserialize` would silently overwrite duplicate keys,
/// hiding them from `validate_named_items`. Duplicate VALUES are
/// intentionally permitted: one producer/peer may serve multiple `link_id`
/// slots. Value-shape rules are the value type's own concern
/// ([`BindingTargets`] for bindings; [`deserialize_pairings`] checks its
/// `String` targets). Sentinel-key checks live in
/// `PeppyLauncher::deserialize` where the owning `instance_id` is in
/// scope for the structured error.
fn deserialize_kv_entries<'de, D, V>(
    deserializer: D,
    kind: &'static str,
) -> Result<Vec<(String, V)>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    let entries =
        deserializer.deserialize_map(BindingEntriesVisitor::<V>(std::marker::PhantomData))?;
    validate_named_items(entries.iter().map(|(k, _)| k.as_str()), kind)
        .map_err(de::Error::custom)?;
    Ok(entries)
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

struct BindingEntriesVisitor<V>(std::marker::PhantomData<V>);

impl<'de, V> Visitor<'de> for BindingEntriesVisitor<V>
where
    V: Deserialize<'de>,
{
    type Value = Vec<(String, V)>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a map of link_id -> target entries")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some((key, value)) = access.next_entry::<String, V>()? {
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
            backbone.bindings.get("torso_camera").map(|t| t.targets()),
            Some(&["cam_torso".to_string()][..])
        );
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
    /// concurrent `link_ids` on the wire. Duplicates across DIFFERENT
    /// keys are therefore intentionally permitted (duplicates within one
    /// key's array are not — see
    /// `bindings_reject_duplicate_elements_within_one_key`).
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
            instance.bindings.get("a").map(|t| t.targets()),
            Some(&["cam_torso".to_string()][..])
        );
        assert_eq!(
            instance.bindings.get("b").map(|t| t.targets()),
            Some(&["cam_torso".to_string()][..])
        );
    }

    /// A binding value may be an array of producer instance_ids (a
    /// from_any slot bound to several producers). Order is preserved,
    /// and serialization keeps the array form for multi-target values
    /// while collapsing single-target values back to the scalar form.
    #[test]
    fn bindings_accept_array_values_and_round_trip() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: {
                arm_states: ["left_arm", "right_arm"],
                proximity: "backbone",
            }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("array binding should parse");
        assert_eq!(
            instance.bindings.get("arm_states").map(|t| t.targets()),
            Some(&["left_arm".to_string(), "right_arm".to_string()][..])
        );
        assert_eq!(
            instance.bindings.get("proximity").map(|t| t.targets()),
            Some(&["backbone".to_string()][..])
        );

        let serialized = serde_json5::to_string(&instance).unwrap();
        assert!(
            serialized.contains(r#"["left_arm","right_arm"]"#),
            "multi-target keeps array form: {serialized}"
        );
        assert!(
            serialized.contains(r#""proximity":"backbone""#),
            "single target collapses to scalar form: {serialized}"
        );
        let reparsed: DeploymentInstance = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.bindings, instance.bindings);
    }

    /// An empty array is legal and equivalent to omitting the key: the
    /// from_any slot stays deliberately unbound (silent).
    #[test]
    fn bindings_accept_empty_array_as_deliberately_unbound() {
        let json5 = r#"{
            instance_id: "backbone",
            bindings: { commander: [] }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("empty array binding should parse");
        let targets = instance.bindings.get("commander").expect("key present");
        assert!(targets.is_empty());
    }

    /// Duplicate elements within one key's array are rejected at parse
    /// time — one producer cannot be bound to the same slot twice.
    #[test]
    fn bindings_reject_duplicate_elements_within_one_key() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: { arm_states: ["left_arm", "left_arm"] }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("duplicate array element must be rejected");
        assert!(
            err.to_string().contains("duplicate"),
            "unexpected error: {err}"
        );
    }

    /// Empty-string elements inside an array are rejected exactly like
    /// an empty scalar value.
    #[test]
    fn bindings_reject_empty_element_in_array() {
        let json5 = r#"{
            instance_id: "commander",
            bindings: { arm_states: ["left_arm", ""] }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty array element must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    /// The launcher-level unknown-instance cross-check runs per element
    /// of an array value.
    #[test]
    fn bindings_reject_unknown_instance_id_inside_array() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { local: "./left" },
                    instances: [{ instance_id: "left_arm" }]
                },
                {
                    source: { local: "./commander" },
                    instances: [{
                        instance_id: "commander",
                        bindings: { arm_states: ["left_arm", "ghost_arm"] }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("unknown array element must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            binding,
            instance_id,
        } = parsing_err
        else {
            panic!("expected UnknownInstanceId, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "commander");
        assert_eq!(binding, "arm_states");
        assert_eq!(instance_id, "ghost_arm");
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
