use crate::error::StructuredError;
use crate::internal::contract::validate_named_items;
use crate::internal::core_node_name::{CoreNodeName, SELF_CORE_NODE};
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
    /// Named placeholders for the machines this launcher spans, wired to
    /// concrete federated core nodes at launch time by
    /// `stack launch --place <core-node-link>@<core-node>`.
    ///
    /// The file describes a TOPOLOGY; the command binds it to today's
    /// hardware. That separation is why the launcher names
    /// `cloud_inference` rather than a machine: the same file works against a
    /// rented accelerator today and your own rack tomorrow, and `--local`
    /// collapses the whole thing onto one workstation.
    ///
    /// Empty for a single-machine launcher, in which case no instance may name
    /// a `core_node`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_nodes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<Deployment>,
}

/// Custom deserialization for [`PeppyLauncher`] that, after the default
/// shape parse, cross-checks every `links` target against the
/// set of `instance_id`s declared across all deployments. A link that
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
            core_nodes: Vec<String>,
            #[serde(default)]
            deployments: Vec<Deployment>,
        }

        let raw = RawPeppyLauncher::deserialize(deserializer)?;

        validate_core_node_links(&raw.core_nodes).map_err(de::Error::custom)?;

        // A `core_node` must name a declared link. Checking it here, once the
        // whole document is parsed, is what lets the error say WHICH links
        // were available instead of just that this one was not one of them.
        let declared: HashSet<&str> = raw.core_nodes.iter().map(String::as_str).collect();
        for deployment in &raw.deployments {
            for instance in &deployment.instances {
                let Some(core_node) = &instance.core_node else {
                    continue;
                };
                if declared.contains(core_node.as_str()) {
                    continue;
                }
                return Err(de::Error::custom(undeclared_core_node_message(
                    instance.instance_id.as_str(),
                    core_node,
                    &raw.core_nodes,
                )));
            }
        }

        let known_ids: HashSet<&str> = raw
            .deployments
            .iter()
            .flat_map(|d| d.instances.iter())
            .map(|i| i.instance_id.as_str())
            .collect();

        for deployment in &raw.deployments {
            for instance in &deployment.instances {
                for (link, value) in &instance.links {
                    if link == DEFAULT_LINK_ID_SENTINEL {
                        let err = StructuredError::LinkSentinelKey {
                            owner_instance_id: instance.instance_id.to_string(),
                            link: link.clone(),
                        };
                        return Err(de::Error::custom(err.json5_message()));
                    }
                    // Only the instance part of a target names a deployed
                    // instance; a pairing/observer target's optional
                    // `/<link_id>` suffix selects a slot on that instance and
                    // is resolved (against the manifest) at plan time.
                    for target in value.targets() {
                        let (target_instance, _link_suffix) = split_link_target(target);
                        if !known_ids.contains(target_instance) {
                            let err = StructuredError::UnknownInstanceId {
                                owner_instance_id: instance.instance_id.to_string(),
                                link: link.clone(),
                                instance_id: target_instance.to_string(),
                            };
                            return Err(de::Error::custom(err.json5_message()));
                        }
                    }
                }
            }
        }

        Ok(PeppyLauncher {
            peppy_schema: raw.peppy_schema,
            core_nodes: raw.core_nodes,
            deployments: raw.deployments,
        })
    }
}

/// Core node link ids are unique, non-empty, and never the reserved `self`.
///
/// `self` is refused because it already means "the coordinator" at the
/// `--place` surface; a link that was also called `self` would make
/// `--place self@self` parse as something, and nothing about it would tell a
/// reader which `self` was which.
fn validate_core_node_links(core_nodes: &[String]) -> Result<(), String> {
    let mut seen = HashSet::with_capacity(core_nodes.len());
    for link_id in core_nodes {
        if link_id.is_empty() {
            return Err("`core_nodes` contains an empty core node link name".to_owned());
        }
        if CoreNodeName::is_self_keyword(link_id) {
            return Err(format!(
                "`core_nodes` declares `{SELF_CORE_NODE}`, which is reserved: it names the \
                 daemon a launch is sent to, so it cannot also be a placeholder. Rename the \
                 link and wire it with `--place <name>@{SELF_CORE_NODE}` instead."
            ));
        }
        if !seen.insert(link_id.as_str()) {
            return Err(format!(
                "`core_nodes` declares `{link_id}` more than once; core node link names must \
                 be unique"
            ));
        }
    }
    Ok(())
}

/// Explains an instance placed on a link the launcher never declared, listing
/// what it could have named. The two failure shapes are different problems: a
/// document with no `core_nodes` at all is single-machine and the field simply
/// does not apply, while one with a list has a typo or a missing entry.
fn undeclared_core_node_message(instance_id: &str, core_node: &str, declared: &[String]) -> String {
    if declared.is_empty() {
        return format!(
            "instance `{instance_id}` sets `core_node: \"{core_node}\"`, but this launcher \
             declares no `core_nodes`. Add a `core_nodes: [\"{core_node}\"]` list naming the \
             machines this launcher spans, then wire it at launch with \
             `--place {core_node}@<core-node>`."
        );
    }
    format!(
        "instance `{instance_id}` sets `core_node: \"{core_node}\"`, which is not a declared \
         core node link. This launcher declares: {}",
        crate::error::format_quoted_list(declared)
    )
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
            links: BTreeMap::new(),
            defer_links: Vec::new(),
            core_node: None,
        }
    }
}

/// One `links:` value: the target(s) selected for a declared slot,
/// remembering the shape they arrived in. Every launcher link kind (a
/// producer binding, a pairing, or an observer) shares this value type.
///
/// A producer binding's shape mirrors its slot's declared cardinality, but
/// the launch parser has no manifest knowledge, so both launch-file shapes
/// parse everywhere and plan-time validation enforces shape-vs-kind (a
/// pairing or observer slot takes a single scalar target; a producer slot's
/// shape must match its cardinality). Shape-local rules (empty-string
/// targets, duplicate targets within one array, malformed `/` grammar) still
/// fail at parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkValue {
    /// `camera: "front_camera"` / `arm: "arm_1/controller"`: the launch-file
    /// scalar shape. Valid on a `cardinality: "one"` producer slot and on
    /// every pairing/observer slot (whose single target may carry a
    /// `/<link_id>` disambiguation suffix).
    Scalar(String),
    /// `camera: ["front_camera", "rear_camera"]`: the launch-file array
    /// shape, valid only on `one_or_more` / `zero_or_more` producer slots
    /// (where `[]` is a valid definition for `zero_or_more`).
    Array(LinkTargets),
    /// Accumulated `--link camera@front --link camera@rear` occurrences in
    /// flag order. Flag repetition carries no scalar/array shape, so the
    /// validator checks it against the slot's cardinality by count alone.
    /// Built by the CLI; never parsed from a launch file. Non-empty by
    /// construction (zero occurrences is an omitted link).
    Flags(LinkTargets),
}

impl LinkValue {
    /// The target ids in declaration order, shape-erased.
    pub fn targets(&self) -> &[String] {
        match self {
            LinkValue::Scalar(target) => std::slice::from_ref(target),
            LinkValue::Array(targets) | LinkValue::Flags(targets) => targets.as_slice(),
        }
    }

    /// The single target of a pairing/observer link, or `None` when the value
    /// carries a set of targets. Pairing and observer slots take exactly one
    /// `<instance>[/<link_id>]` target, so their validators call this to reject
    /// a multi-target value up front. A launch-file scalar and a single CLI
    /// `--link KEY@target` occurrence (a one-element [`LinkValue::Flags`]) both
    /// count as one target; an array or a repeated flag does not.
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            LinkValue::Scalar(target) => Some(target),
            LinkValue::Flags(targets) if targets.len() == 1 => Some(&targets.as_slice()[0]),
            LinkValue::Array(_) | LinkValue::Flags(_) => None,
        }
    }
}

/// The target list of a [`LinkValue::Array`] / [`LinkValue::Flags`]
/// value, duplicate-free by construction: every path that builds one (the
/// launch-file value parser, CLI flag accumulation, programmatic plan
/// building) funnels through [`LinkTargets::new`], so a slot's bound
/// set naming a producer twice is unrepresentable rather than re-checked
/// at each boundary. Declaration order is preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LinkTargets(Vec<String>);

impl LinkTargets {
    /// Parses a raw target list, rejecting the first target that appears
    /// more than once within it.
    pub fn new(targets: Vec<String>) -> Result<Self, DuplicateLinkTarget> {
        let duplicate = {
            let mut seen = HashSet::with_capacity(targets.len());
            targets
                .iter()
                .find(|target| !seen.insert(target.as_str()))
                .cloned()
        };
        if let Some(target) = duplicate {
            return Err(DuplicateLinkTarget { target });
        }
        Ok(Self(targets))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Error from [`LinkTargets::new`]: `target` appears more than once
/// within one slot's set. Boundaries prefix it with their own surface
/// context (the link key at launch-file parse, the `--link` flag pair
/// on the CLI); the rule sentence itself is stated only here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateLinkTarget {
    pub target: String,
}

impl std::fmt::Display for DuplicateLinkTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "names target `{}` more than once: a slot's bound set lists each producer once",
            self.target
        )
    }
}

impl std::error::Error for DuplicateLinkTarget {}

/// Serializes back to the launch-file shapes: `Scalar` as a string,
/// `Array` as an array. `Flags` also serializes as an array; it exists
/// only on CLI-built plans, which are never round-tripped through a launch
/// file, and the array form is its closest document equivalent.
impl Serialize for LinkValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            LinkValue::Scalar(target) => serializer.serialize_str(target),
            LinkValue::Array(targets) | LinkValue::Flags(targets) => targets.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for LinkValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LinkValueVisitor;

        impl<'de> de::Visitor<'de> for LinkValueVisitor {
            type Value = LinkValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str("a target instance_id string or an array of instance_id strings")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LinkValue::Scalar(v.to_string()))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut targets: Vec<String> = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(target) = seq.next_element::<String>()? {
                    targets.push(target);
                }
                let targets = LinkTargets::new(targets).map_err(de::Error::custom)?;
                Ok(LinkValue::Array(targets))
            }
        }

        deserializer.deserialize_any(LinkValueVisitor)
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
    /// The unified per-instance link map: own `link_id` → target(s). One
    /// key namespace covers all three link kinds, disambiguated at plan time
    /// against the node's `depends_on`:
    ///   - a producer slot (`depends_on.{nodes,contracts}`) takes a scalar or
    ///     an array of producer `instance_id`s per its cardinality;
    ///   - a participant pairing slot takes a single peer target
    ///     (`"<instance_id>"` or `"<instance_id>/<peer_link_id>"` to
    ///     disambiguate) — declaring the pair on ONE side covers both
    ///     endpoints' slots;
    ///   - an observer slot takes a single source target
    ///     (`"<source_instance>"` or `"<source_instance>/<source_link_id>"`).
    ///
    /// The launch parser has no manifest knowledge, so shape is validated
    /// against slot kind at plan time; only shape-local rules (empty targets,
    /// duplicates within one array, malformed `/` grammar) fail at parse.
    #[serde(
        default,
        deserialize_with = "deserialize_links",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub links: BTreeMap<String, LinkValue>,
    /// Required pairing/observer slots deliberately left unresolved at launch.
    /// Every required participant slot must be paired or listed here
    /// (`PairingSlotUncovered` otherwise) and every observer slot must be
    /// linked or listed here (`ObservationSlotUncovered` otherwise). Optional
    /// participant slots need no entry, and producer-binding slots cannot be
    /// deferred (`LinkDeferInvalid`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defer_links: Vec<String>,
    /// Which declared core node link this instance is placed on, i.e. which
    /// machine it runs on once `--place` has bound that link to a real core
    /// node.
    ///
    /// OPTIONAL: an instance that omits it runs on the coordinator, the daemon
    /// the launch was sent to.
    ///
    /// Deliberately a SEPARATE field from `instance_id` rather than a prefix
    /// on it. Identity and placement are two different facts, so they get two
    /// differently-typed fields: `instance_id` keeps its existing charset
    /// untouched, nothing anywhere has to split a placement back out of an
    /// instance id to use one, and a `links:` target stays a bare instance id
    /// wherever it appears. Placement is declared once, here, and never
    /// repeated at the point of use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_node: Option<String>,
}

/// The per-instance `links:` map: each key is a `link_id` literal declared
/// by the deployed node's `depends_on` (a producer slot under
/// `{nodes,contracts}`, or a participant/observer slot under `pairings`), and
/// each value selects the slot's target(s) (see [`LinkValue`]). Every shape
/// parses here because the launch parser has no manifest knowledge; whether a
/// value's shape matches its slot kind is enforced at plan time. Shape-local
/// rules fail at parse: keys are validated for non-emptiness and
/// intra-collection duplicates via [`validate_named_items`], targets must be
/// non-empty strings with well-formed `<instance>[/<link_id>]` grammar, and a
/// target appearing more than once within one slot's array is rejected by
/// [`LinkTargets`] as the value parses. The reserved producer-default
/// sentinel ([`DEFAULT_LINK_ID_SENTINEL`]) is rejected as a key, and each
/// target's instance existence is checked, at the [`PeppyLauncher`] level once
/// all deployments have been parsed.
fn deserialize_links<'de, D>(deserializer: D) -> Result<BTreeMap<String, LinkValue>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries = deserializer.deserialize_map(LinkEntriesVisitor)?;
    validate_named_items(entries.iter().map(|(k, _)| k.as_str()), "link")
        .map_err(de::Error::custom)?;
    let mut out = BTreeMap::new();
    for (key, value) in entries {
        for target in value.targets() {
            if target.trim().is_empty() {
                return Err(de::Error::custom(format!(
                    "link target for key `{key}` cannot be empty"
                )));
            }
            // Reject malformed `/` grammar at parse time (kind-agnostic:
            // producer targets never carry a suffix, pairing/observer
            // targets carry at most one). A bad suffix here would otherwise
            // surface later as a confusing "no complementary slot" error.
            let (instance, link_suffix) = split_link_target(target);
            if instance.is_empty() || link_suffix.is_some_and(|l| l.is_empty() || l.contains('/')) {
                return Err(de::Error::custom(format!(
                    "link target `{target}` for key `{key}` is malformed: expected \
                     `<instance>` or `<instance>/<link_id>`"
                )));
            }
        }
        out.insert(key, value);
    }
    Ok(out)
}

/// Splits a launcher `links` scalar target (or CLI `--link` right-hand side)
/// into `(instance_id, Option<link_id>)`. The `/` separator cannot appear
/// inside wire segments, so the split is unambiguous.
pub fn split_link_target(value: &str) -> (&str, Option<&str>) {
    match value.split_once('/') {
        Some((instance, link)) => (instance, Some(link)),
        None => (value, None),
    }
}

/// Map visitor for the unified `links` deserializer. Collects into a Vec so
/// duplicate keys survive to `validate_named_items` instead of being collapsed
/// by a map insert.
struct LinkEntriesVisitor;

impl<'de> Visitor<'de> for LinkEntriesVisitor {
    type Value = Vec<(String, LinkValue)>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a map of link_id -> target instance_id(s)")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(key) = access.next_key::<String>()? {
            let value = access
                .next_value::<LinkValue>()
                .map_err(|err| de::Error::custom(format!("link `{key}`: {err}")))?;
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

    /// Test shorthand: an `Array` binding value from unique literals.
    fn array(targets: &[&str]) -> LinkValue {
        LinkValue::Array(
            LinkTargets::new(targets.iter().map(|t| t.to_string()).collect())
                .expect("test targets are unique"),
        )
    }

    /// The each-producer-once rule lives in `LinkTargets::new`, the one
    /// constructor every boundary (launch-file parse, CLI flags,
    /// programmatic plan building) funnels through, so no path can build a
    /// bound set naming a producer twice.
    #[test]
    fn binding_targets_reject_duplicates_at_construction() {
        let err = LinkTargets::new(vec![
            "prod1".to_string(),
            "prod2".to_string(),
            "prod1".to_string(),
        ])
        .expect_err("duplicate target must be rejected");
        assert_eq!(err.target, "prod1");
        assert!(
            err.to_string().contains("once"),
            "error must state the each-producer-once rule: {err}"
        );

        let targets = LinkTargets::new(vec!["prod1".to_string(), "prod2".to_string()])
            .expect("unique targets construct");
        assert_eq!(targets.as_slice(), ["prod1", "prod2"]);
    }

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
                        links: {
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
        assert_eq!(backbone.links.len(), 3);
        assert_eq!(
            backbone.links.get("torso_camera"),
            Some(&LinkValue::Scalar("cam_torso".to_string()))
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
            links: {
                main: "camera_inst",
                arm_states: ["right_arm_inst", "left_arm_inst"],
                spare_cameras: []
            }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("both shapes should parse");
        assert_eq!(
            instance.links.get("main"),
            Some(&LinkValue::Scalar("camera_inst".to_string()))
        );
        assert_eq!(
            instance.links.get("arm_states"),
            Some(&array(&["right_arm_inst", "left_arm_inst"])),
            "array order must be preserved, not sorted"
        );
        assert_eq!(instance.links.get("spare_cameras"), Some(&array(&[])));
    }

    /// Shape-local parse rule: the same target twice within one slot's
    /// array is rejected at parse, naming the slot and the target.
    #[test]
    fn bindings_reject_duplicate_targets_within_one_slot() {
        let json5 = r#"{
            instance_id: "commander",
            links: { arm_states: ["arm_inst", "other_inst", "arm_inst"] }
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
            links: { arm_states: ["arm_inst", ""] }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty target inside an array must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn bindings_default_to_empty_when_omitted() {
        let instance: DeploymentInstance =
            serde_json5::from_str("{ instance_id: \"camera_front\" }").unwrap();
        assert!(instance.links.is_empty());
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
                        links: {
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
            link,
            instance_id,
        } = parsing_err
        else {
            panic!("expected UnknownInstanceId, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "backbone");
        assert_eq!(link, "torso_camera");
        assert_eq!(instance_id, "does_not_exist");
    }

    #[test]
    fn bindings_reject_empty_key() {
        let json5 = r#"{
            instance_id: "backbone",
            links: { "": "cam_torso" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(json5)
            .expect_err("empty binding key must be rejected");
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn bindings_reject_empty_value() {
        let json5 = r#"{
            instance_id: "backbone",
            links: { torso_camera: "" }
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
            links: {
                a: "cam_torso",
                b: "cam_torso"
            }
        }"#;
        let instance: DeploymentInstance =
            serde_json5::from_str(json5).expect("duplicate binding targets should now be accepted");
        assert_eq!(
            instance.links.get("a"),
            Some(&LinkValue::Scalar("cam_torso".to_string()))
        );
        assert_eq!(
            instance.links.get("b"),
            Some(&LinkValue::Scalar("cam_torso".to_string()))
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
                        links: { "_": "backbone" }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("`_` link key must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::LinkSentinelKey {
            owner_instance_id,
            link,
        } = &parsing_err
        else {
            panic!("expected LinkSentinelKey, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "backbone");
        assert_eq!(link, "_");
    }

    /// The `pairings` map parses, resolves against siblings, and supports
    /// the `/<peer_link_id>` disambiguation suffix; `defer_links` rides
    /// alongside.
    #[test]
    fn links_and_defer_links_parse() {
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
                        links: { arm: "arm_1" },
                        defer_links: ["spare"]
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher = serde_json5::from_str(json5).expect("launcher should parse");
        let ctrl = &launcher.deployments[1].instances[0];
        assert_eq!(
            ctrl.links.get("arm").and_then(LinkValue::as_scalar),
            Some("arm_1")
        );
        assert_eq!(ctrl.defer_links, vec!["spare".to_string()]);

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
                        links: { arm: "arm_1/controller" }
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher =
            serde_json5::from_str(json5).expect("suffixed pairing should parse");
        let ctrl = &launcher.deployments[1].instances[0];
        assert_eq!(
            ctrl.links.get("arm").and_then(LinkValue::as_scalar),
            Some("arm_1/controller")
        );
        assert_eq!(
            split_link_target("arm_1/controller"),
            ("arm_1", Some("controller"))
        );
        assert_eq!(split_link_target("arm_1"), ("arm_1", None));
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
                        links: { arm: "ghost/controller" }
                    }]
                }
            ]
        }"#;
        let err = serde_json5::from_str::<PeppyLauncher>(json5)
            .expect_err("unknown pairing target must be rejected");
        let parsing_err = ParsingError::from(err);
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            link,
            instance_id,
        } = parsing_err
        else {
            panic!("expected UnknownInstanceId, got {parsing_err:?}");
        };
        assert_eq!(owner_instance_id, "ctrl_1");
        assert_eq!(link, "arm");
        assert_eq!(instance_id, "ghost");
    }

    #[test]
    fn pairings_reject_duplicate_and_empty_entries() {
        let dup = r#"{
            instance_id: "ctrl_1",
            links: { "arm": "a1", "arm": "a2" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(dup)
            .expect_err("duplicate pairing key must be rejected");
        assert!(err.to_string().contains("duplicate"), "error: {err}");

        let empty_value = r#"{
            instance_id: "ctrl_1",
            links: { "arm": "" }
        }"#;
        let err = serde_json5::from_str::<DeploymentInstance>(empty_value)
            .expect_err("empty pairing target must be rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");

        let empty_key = r#"{
            instance_id: "ctrl_1",
            links: { "": "arm_1" }
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
                    links: {{ "arm": "{target}" }}
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
            links: { "main": "prod_a", "main": "prod_b" }
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

/// Where every instance of one launch runs.
///
/// Producer addresses are `(core_node, instance_id)` pairs on the wire, so the
/// validators need to stamp each resolved binding with the core node its
/// PRODUCER sits on, not the one the launch was sent to. Before federation
/// those were always the same daemon and a single `&str` sufficed; now a
/// consumer on one machine can be bound to a producer on another, so the stamp
/// is per instance.
///
/// Note what this does NOT change: a `links:` target is still a bare instance
/// id. Placement is declared once on the instance and looked up here, so
/// nothing at the point of use records which machine a producer sits on.
///
/// Every name in here is a [`CoreNodeName`], which is what makes placement the
/// point where core-node names are checked. A launch goal arrives over the wire
/// carrying whatever its sender put in it; the CLI validates what a user types,
/// but the daemon cannot assume its caller was the CLI. Taking parsed names
/// means an unchecked one cannot reach a `Placements` at all, rather than each
/// consumer being trusted to re-check.
#[derive(Debug, Clone)]
pub struct Placements {
    /// Where an instance that declared no `core_node` runs: the coordinator,
    /// i.e. the daemon the launch was sent to.
    coordinator: CoreNodeName,
    by_instance: BTreeMap<String, CoreNodeName>,
}

impl Placements {
    /// Every instance on one daemon. The single-machine case, and the shape
    /// every non-federated path (`node run`, a launcher with no `core_nodes`)
    /// uses.
    pub fn all_on(core_node: CoreNodeName) -> Self {
        Self {
            coordinator: core_node,
            by_instance: BTreeMap::new(),
        }
    }

    /// Placements resolved from a launcher document and its `--place` wiring.
    /// `by_instance` holds only the instances that named a `core_node`;
    /// everything else falls back to the coordinator.
    pub fn new(coordinator: CoreNodeName, by_instance: BTreeMap<String, CoreNodeName>) -> Self {
        Self {
            coordinator,
            by_instance,
        }
    }

    /// The core node `instance_id` runs on.
    pub fn of(&self, instance_id: &str) -> &str {
        self.by_instance
            .get(instance_id)
            .unwrap_or(&self.coordinator)
            .as_str()
    }

    /// Whether any instance is placed off the coordinator, i.e. whether this
    /// launch actually spans machines.
    pub fn is_federated(&self) -> bool {
        self.by_instance
            .values()
            .any(|core_node| core_node != &self.coordinator)
    }
}
