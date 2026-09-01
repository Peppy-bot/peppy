use super::composition::{Adjustment, ComponentAxis, SelectionConstraint};
use crate::error::StructuredError;
use crate::internal::contract::validate_named_items;
use crate::internal::core_node_name::{CoreNodeName, SELF_CORE_NODE};
use config::{AnyType, consts::DEFAULT_LINK_ID_SENTINEL, runtime::Name, schema::PeppySchema};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub use crate::internal::source::DeploymentSource;

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
    /// The component axes of a composed launcher: what `--with` selects
    /// between. Empty for a flat stack, which is the ordinary way to write a
    /// one-off.
    ///
    /// A launcher that declares axes is a FAMILY of stacks, not one: its base
    /// `deployments` may link instance ids only an option defines, so the
    /// whole-document cross-checks a flat launcher gets below (every link
    /// target names a known instance, every `core_node` names a declared
    /// link) are deferred to the flattened result, where the selected
    /// options' deployments are part of the document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<ComponentAxis>,
    /// The base's changes to instances defined elsewhere, applied after all
    /// fragment adjustments. How a base specializes fragments shared between
    /// launchers. Requires `components`: with nothing to specialize, an
    /// adjustment is indirection around a file the author can edit directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adjustments: Vec<Adjustment>,
    /// The selections this family refuses to be: rules that a resolved
    /// selection must satisfy or the launch is refused before anything is
    /// pinned or started. How a family excludes members that would flatten
    /// cleanly into a stack nobody should run. Requires `components`: with
    /// nothing to select there is no selection to refuse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<SelectionConstraint>,
}

/// Custom deserialization for [`PeppyLauncher`] that, after the default
/// shape parse, cross-checks every `links` target against the
/// set of `instance_id`s declared across all deployments. A link that
/// points at an unknown instance is rejected with a structured
/// [`StructuredError::UnknownInstanceId`] so callers see a path-aware
/// message instead of a generic serde error.
///
/// A COMPOSED launcher (one declaring `components`) gets the same shape
/// checks per fragment but not the cross-instance ones: its base links may
/// name ids only a selected option defines, so those checks run once, on
/// the flattened document.
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
            #[serde(default)]
            components: Vec<ComponentAxis>,
            #[serde(default)]
            adjustments: Vec<Adjustment>,
            #[serde(default)]
            constraints: Vec<SelectionConstraint>,
        }

        let raw = RawPeppyLauncher::deserialize(deserializer)?;

        validate_core_node_links(&raw.core_nodes).map_err(de::Error::custom)?;
        super::composition::validate_axes(&raw.components).map_err(de::Error::custom)?;
        // Checked before the per-adjustment validation: a flat launcher with
        // adjustments should hear that adjustments do not belong here, not
        // that one of its guards names an axis the (empty) `components` list
        // does not declare.
        if raw.components.is_empty() && !raw.adjustments.is_empty() {
            return Err(de::Error::custom(
                "this launcher declares `adjustments` but no `components`: with nothing to \
                 specialize, an adjustment is indirection around a file the author can edit \
                 directly. Move the values into the instances themselves, or declare the axes \
                 the adjustments specialize",
            ));
        }
        super::composition::validate_launcher_adjustments(&raw.adjustments, &raw.components)
            .map_err(de::Error::custom)?;
        // Same shape of refusal as the adjustments one above: a constraint
        // speaks in axis and option names, so without axes it refers to
        // nothing.
        if raw.components.is_empty() && !raw.constraints.is_empty() {
            return Err(de::Error::custom(
                "this launcher declares `constraints` but no `components`: with nothing to \
                 select there is no selection to refuse. Declare the axes the constraints \
                 speak about, or drop them",
            ));
        }
        super::composition::validate_constraints(&raw.constraints, &raw.components)
            .map_err(de::Error::custom)?;

        if raw.components.is_empty() {
            cross_check_flat_document(&raw.deployments, &raw.core_nodes)
                .map_err(de::Error::custom)?;
        }

        Ok(PeppyLauncher {
            peppy_schema: raw.peppy_schema,
            core_nodes: raw.core_nodes,
            deployments: raw.deployments,
            components: raw.components,
            adjustments: raw.adjustments,
            constraints: raw.constraints,
        })
    }
}

/// The whole-document cross-checks of a flat launcher: every `core_node`
/// names a declared link, no link key is the reserved producer-default
/// sentinel, and every link target names a known instance. Extracted from
/// [`PeppyLauncher`]'s deserializer so the composed arm of that deserializer
/// can defer exactly these checks to the flattened document.
fn cross_check_flat_document(
    deployments: &[Deployment],
    core_nodes: &[String],
) -> Result<(), String> {
    // A `core_node` must name a declared link. Checking it here, once the
    // whole document is parsed, is what lets the error say WHICH links
    // were available instead of just that this one was not one of them.
    let declared: HashSet<&str> = core_nodes.iter().map(String::as_str).collect();
    for deployment in deployments {
        for instance in &deployment.instances {
            let Some(core_node) = &instance.core_node else {
                continue;
            };
            if declared.contains(core_node.as_str()) {
                continue;
            }
            return Err(undeclared_core_node_message(
                instance.instance_id.as_str(),
                core_node,
                core_nodes,
            ));
        }
    }

    let known_ids: HashSet<&str> = deployments
        .iter()
        .flat_map(|d| d.instances.iter())
        .map(|i| i.instance_id.as_str())
        .collect();

    for deployment in deployments {
        for instance in &deployment.instances {
            for (link, value) in &instance.links {
                if link == DEFAULT_LINK_ID_SENTINEL {
                    let err = StructuredError::LinkSentinelKey {
                        owner_instance_id: instance.instance_id.to_string(),
                        link: link.clone(),
                    };
                    return Err(err.json5_message());
                }
                // Only the instance part of a target names a deployed
                // instance; a pairing/observer target's optional
                // `/<link_id>` suffix selects a slot on that instance and
                // is resolved (against the manifest) at plan time. A vacant
                // slot selects nothing, so it names no instance to check.
                let Some(selection) = value.selection() else {
                    continue;
                };
                for target in selection.targets() {
                    let (target_instance, _link_suffix) = split_link_target(target);
                    if !known_ids.contains(target_instance) {
                        let err = StructuredError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            link: link.clone(),
                            instance_id: target_instance.to_string(),
                        };
                        return Err(err.json5_message());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Core node link ids are unique, never the reserved `self`, and spelled like
/// the core node names they stand in for.
///
/// `self` is refused because it already means "the coordinator" at the
/// `--place` surface; a link that was also called `self` would make
/// `--place self@self` parse as something, and nothing about it would tell a
/// reader which `self` was which.
///
/// The rest of the grammar comes from [`CoreNodeName`] rather than being
/// invented here. A link id is one half of `--place <link>@<core-node>`, so a
/// name carrying a space, a `/`, or an `@` would be unwireable from the very
/// surface it exists for, and the two halves having different rules is a
/// distinction no author could guess.
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
        CoreNodeName::new(link_id.as_str())
            .map_err(|reason| format!("`core_nodes` declares `{link_id}`, which {reason}"))?;
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
    /// already-running instances. The default-empty `framework` reports the
    /// instance as no simulated-time source, which is what an instance
    /// started by an earlier launch is as far as this one is concerned.
    pub fn empty(instance_id: Name) -> Self {
        Self {
            instance_id,
            arguments: BTreeMap::new(),
            env_vars: BTreeMap::new(),
            framework: FrameworkOverrides::default(),
            links: BTreeMap::new(),
            core_node: None,
        }
    }
}

/// One `links:` value: either a binding to real targets, or a deliberate
/// non-binding carrying the reason the slot stays empty. Every launcher link
/// kind (a producer binding, a pairing, or an observer) shares this value
/// type, so the fate of a declared slot is one entry in one map.
///
/// Vacancy is NOT a binding, so it is a variant of this enum rather than a
/// fourth shape inside [`Selection`]: a consumer that wants targets must
/// first hold a [`Selection`], and no validation order can let a vacancy
/// answer for a target set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkValue {
    /// `arm: "arm_1"` / `cameras: ["front", "rear"]`: the slot's selected
    /// targets.
    Bound(Selection),
    /// `leader_left_arm: { vacant: "monitor rig: nothing commands this
    /// backbone" }`: the slot boots unresolved on purpose, and the deployment
    /// says why. Legal only where the node's own manifest declares the slot
    /// emptiable: `optional: true` on a participant pairing slot, or
    /// `cardinality: "zero_or_one"` on an observer slot or a producer-binding
    /// slot. A slot the manifest declares required cannot be vacated at all,
    /// and a multi-cardinality slot writes its emptiness as `[]` or an omitted
    /// key; [`crate::launcher::links::validate_link_slots`] rejects a vacancy
    /// on either.
    Vacant(VacantReason),
}

/// The target(s) selected for a declared slot, remembering the shape they
/// arrived in.
///
/// A producer binding's shape mirrors its slot's declared cardinality, but
/// the launch parser has no manifest knowledge, so both launch-file shapes
/// parse everywhere and plan-time validation enforces shape-vs-kind (a
/// pairing or observer slot takes a single scalar target; a producer slot's
/// shape must match its cardinality). Shape-local rules (empty-string
/// targets, duplicate targets within one array, malformed `/` grammar) still
/// fail at parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
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
    /// The slot's selected targets, or `None` when the slot is vacant.
    pub fn selection(&self) -> Option<&Selection> {
        match self {
            LinkValue::Bound(selection) => Some(selection),
            LinkValue::Vacant(_) => None,
        }
    }

    /// Why this slot deliberately binds nothing, or `None` when it binds
    /// targets.
    pub fn vacancy(&self) -> Option<&VacantReason> {
        match self {
            LinkValue::Vacant(reason) => Some(reason),
            LinkValue::Bound(_) => None,
        }
    }
}

impl Selection {
    /// The target ids in declaration order, shape-erased.
    pub fn targets(&self) -> &[String] {
        match self {
            Selection::Scalar(target) => std::slice::from_ref(target),
            Selection::Array(targets) | Selection::Flags(targets) => targets.as_slice(),
        }
    }

    /// The single target of a participant pairing link, or `None` when the
    /// selection carries a set of targets. A pairing is strictly 1:1, so a
    /// participant slot takes exactly one `<instance>[/<link_id>]` target and
    /// its validator calls this to reject a multi-target value up front.
    /// Observer slots are sized by their `cardinality` instead and go through
    /// [`check_cardinality_shape`]. A launch-file scalar and a
    /// single CLI `--link KEY@target` occurrence (a one-element
    /// [`Selection::Flags`]) both count as one target; an array or a repeated
    /// flag does not.
    pub fn as_scalar(&self) -> Option<&str> {
        match self {
            Selection::Scalar(target) => Some(target),
            Selection::Flags(targets) if targets.len() == 1 => Some(&targets.as_slice()[0]),
            Selection::Array(_) | Selection::Flags(_) => None,
        }
    }
}

/// The `{ vacant: "<why>" }` entries of one instance's `links` map that name a
/// participant pairing slot, in the `link_id -> reason` shape a node-run goal's
/// `vacant_pairs` field carries. Observer and producer vacancies are dropped:
/// each is validated by its own family and neither produces a vacancy record on
/// the goal. A vacant producer slot's goal-side artifact is the explicit empty
/// set its `slot_bindings` entry carries, not a reason the daemon forwards.
pub fn participant_vacancies(
    links: &BTreeMap<String, LinkValue>,
    participant_link_ids: &std::collections::BTreeSet<&str>,
) -> BTreeMap<String, String> {
    links
        .iter()
        .filter(|(link_id, _)| participant_link_ids.contains(link_id.as_str()))
        .filter_map(|(link_id, value)| {
            let reason = value.vacancy()?;
            Some((link_id.clone(), reason.as_str().to_owned()))
        })
        .collect()
}

/// Why a slot is deliberately left unresolved, in the deployment's own words.
/// Non-empty once trimmed: a bare marker says only "not this one", and the
/// point of writing a vacancy down is that a reader who has never seen the
/// schema can tell what it says. Every path that builds one (the launch-file
/// value parser, the CLI's `--vacant-link`, the daemon's goal decoder) funnels
/// through [`VacantReason::new`], so a reasonless vacancy is unrepresentable
/// rather than re-checked at each boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct VacantReason(String);

impl VacantReason {
    /// Parses a raw reason, trimming surrounding whitespace and rejecting one
    /// that says nothing.
    pub fn new(reason: &str) -> Result<Self, EmptyVacantReason> {
        let trimmed = reason.trim();
        if trimmed.is_empty() {
            return Err(EmptyVacantReason);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VacantReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Error from [`VacantReason::new`]. Boundaries prefix it with their own
/// surface context (the link key at launch-file parse, the flag value on the
/// CLI); the rule sentence itself is stated only here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyVacantReason;

impl std::fmt::Display for EmptyVacantReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "a vacancy needs a reason: say why the slot stays empty, e.g. \
             `{ vacant: \"monitor rig: nothing commands this backbone\" }`",
        )
    }
}

impl std::error::Error for EmptyVacantReason {}

/// Why a [`Selection`] does not satisfy its slot's declared `cardinality`,
/// shape-only and vocabulary-free: the caller turns it into the error variant
/// its own slot kind speaks (producer binding or observation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardinalityShapeViolation {
    /// A launch-file array value on a scalar slot, of any length.
    ArrayOnScalarSlot {
        cardinality: config::node::Cardinality,
    },
    /// A launch-file scalar value on a multi slot.
    ScalarOnMultiSlot {
        cardinality: config::node::Cardinality,
    },
    /// An empty target set on a `one_or_more` slot.
    Unmet,
    /// Repeated flag occurrences on a scalar slot.
    SingleSlotMultipleTargets {
        cardinality: config::node::Cardinality,
        target_count: usize,
    },
}

/// Does the selection's shape (launch file) or occurrence count (CLI flags)
/// satisfy the slot's declared cardinality?
///
/// Launch-file shapes are strict (a scalar is only valid on a `one` slot and an
/// array only on a multi slot), while flag occurrences carry no shape and are
/// checked by count alone. CLI-built [`Selection::Flags`] is non-empty (zero
/// occurrences is an omitted link, judged by each family's coverage rule), but
/// an empty programmatic value on `one_or_more` is still rejected.
///
/// Takes a [`Selection`] rather than a [`LinkValue`] because a vacant slot
/// selects nothing and has no shape to size: whether it may be vacant at all is
/// the legality question [`crate::launcher::links::validate_link_slots`]
/// answers, before any of this.
///
/// The one shape rule, shared by producer-binding slots and observer slots so a
/// deployment writes `["a", "b"]` for the same reason on both.
pub fn check_cardinality_shape(
    cardinality: config::node::Cardinality,
    selection: &Selection,
) -> Result<(), CardinalityShapeViolation> {
    use config::node::Cardinality;
    match (cardinality, selection) {
        // `zero_or_one` is scalar-shaped like `one`: the floor between them is
        // a coverage question (an empty `zero_or_one` slot is written vacant,
        // and this function never sees a vacancy), not a shape one, so a
        // present selection is sized identically on both.
        (Cardinality::One | Cardinality::ZeroOrOne, Selection::Scalar(_)) => Ok(()),
        (Cardinality::One | Cardinality::ZeroOrOne, Selection::Array(_)) => {
            Err(CardinalityShapeViolation::ArrayOnScalarSlot { cardinality })
        }
        (Cardinality::One | Cardinality::ZeroOrOne, Selection::Flags(targets)) => {
            if targets.len() == 1 {
                Ok(())
            } else {
                Err(CardinalityShapeViolation::SingleSlotMultipleTargets {
                    cardinality,
                    target_count: targets.len(),
                })
            }
        }
        (Cardinality::OneOrMore | Cardinality::ZeroOrMore, Selection::Scalar(_)) => {
            Err(CardinalityShapeViolation::ScalarOnMultiSlot { cardinality })
        }
        (Cardinality::OneOrMore, Selection::Array(targets) | Selection::Flags(targets))
            if targets.is_empty() =>
        {
            Err(CardinalityShapeViolation::Unmet)
        }
        (
            Cardinality::OneOrMore | Cardinality::ZeroOrMore,
            Selection::Array(_) | Selection::Flags(_),
        ) => Ok(()),
    }
}

/// The target list of a [`Selection::Array`] / [`Selection::Flags`]
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

/// The `vacant` key, the launch-file grammar's only object-valued link.
const VACANT_KEY: &str = "vacant";

/// Serializes back to the launch-file shapes: `Scalar` as a string, `Array` as
/// an array, `Vacant` as `{ vacant: "<reason>" }`. `Flags` also serializes as
/// an array; it exists only on CLI-built plans, which are never round-tripped
/// through a launch file, and the array form is its closest document
/// equivalent.
impl Serialize for LinkValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            LinkValue::Bound(Selection::Scalar(target)) => serializer.serialize_str(target),
            LinkValue::Bound(Selection::Array(targets) | Selection::Flags(targets)) => {
                targets.serialize(serializer)
            }
            LinkValue::Vacant(reason) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(VACANT_KEY, reason)?;
                map.end()
            }
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
                formatter.write_str(
                    "a target instance_id string, an array of instance_id strings, or \
                     { vacant: \"<why>\" }",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(LinkValue::Bound(Selection::Scalar(v.to_string())))
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
                Ok(LinkValue::Bound(Selection::Array(targets)))
            }

            /// `{ vacant: "<why>" }` and nothing else: exactly one key, named
            /// `vacant`, holding a reason that says something.
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut reason: Option<VacantReason> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key != VACANT_KEY {
                        return Err(de::Error::custom(format!(
                            "unknown link key `{key}`: an object link value is \
                             `{{ {VACANT_KEY}: \"<why>\" }}` and takes no other key"
                        )));
                    }
                    if reason.is_some() {
                        return Err(de::Error::duplicate_field(VACANT_KEY));
                    }
                    let raw = map.next_value::<String>()?;
                    reason = Some(VacantReason::new(&raw).map_err(de::Error::custom)?);
                }
                let reason = reason.ok_or_else(|| de::Error::missing_field(VACANT_KEY))?;
                Ok(LinkValue::Vacant(reason))
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
    /// Environment variables handed to this instance's process, layered over
    /// the caller environment the daemon forwards to every node (the instance
    /// wins on a shared key). Validated at parse time against [`crate::env`],
    /// so a launcher that parses carries only entries a node can actually
    /// receive.
    #[serde(default, deserialize_with = "deserialize_env_vars")]
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
    ///   - an observer slot takes source targets
    ///     (`"<source_instance>"` or `"<source_instance>/<source_link_id>"`),
    ///     as a scalar or an array per its cardinality, exactly like a
    ///     producer slot.
    ///
    /// A slot that starts unresolved on purpose says so in this same map, as
    /// `{ vacant: "<why>" }`: the fate of every declared slot is one entry
    /// here, and forgetting a slot is an absence rather than a value. Only a
    /// slot the node's manifest declares emptiable (`optional: true` on a
    /// participant, `cardinality: "zero_or_one"` on an observer or a producer
    /// slot) may take that value.
    ///
    /// The launch parser has no manifest knowledge, so shape is validated
    /// against slot kind at plan time; only shape-local rules (empty targets,
    /// duplicates within one array, malformed `/` grammar, a vacancy with no
    /// reason) fail at parse.
    #[serde(
        default,
        deserialize_with = "deserialize_links",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub links: BTreeMap<String, LinkValue>,
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
        // A vacant slot selects no targets, so the target rules below have
        // nothing to judge; its own rule (a reason that says something) was
        // enforced as the value parsed.
        let Some(selection) = value.selection() else {
            out.insert(key, value);
            continue;
        };
        for target in selection.targets() {
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

/// The per-instance `env_vars` map. Every entry is checked here rather than at
/// spawn time: `peppy stack launch` clears the running stack before it starts
/// anything, so a name or value a node could not receive must fail while the
/// file is being read (the CLI parses it locally first) instead of halfway
/// through a launch that already tore the previous stack down.
fn deserialize_env_vars<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let env_vars = BTreeMap::<String, String>::deserialize(deserializer)?;
    for (name, value) in &env_vars {
        crate::internal::env::check_env_var(name, value).map_err(de::Error::custom)?;
    }
    Ok(env_vars)
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
/// uniformly to every node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameworkOverrides {
    /// Optional so the daemon falls through to its own `--clock-source`
    /// default when the instance omits the override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_sim_time: Option<bool>,
    /// This instance is the launch's one source of simulated time: the daemon
    /// hands it every machine of the launch, and its `SimTimePublisher` feeds
    /// each machine's `clock` topic. At most one instance per launch may say
    /// so; a second is refused when the flattened document is checked, which
    /// is what keeps a fleet on one timeline whatever expanded the document.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub publishes_sim_time: bool,
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
        LinkValue::Bound(Selection::Array(
            LinkTargets::new(targets.iter().map(|t| t.to_string()).collect())
                .expect("test targets are unique"),
        ))
    }

    /// Test shorthand: the single target of a bound scalar link.
    fn scalar_target<'a>(instance: &'a DeploymentInstance, link_id: &str) -> Option<&'a str> {
        instance
            .links
            .get(link_id)
            .and_then(LinkValue::selection)
            .and_then(Selection::as_scalar)
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
            source: { name: "uvc_camera:v1" },
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

    /// An `env_vars` entry a node could not receive intact is rejected while
    /// the launcher is being parsed, and the message names the variable so the
    /// user knows which line to fix.
    #[test]
    fn env_vars_reject_entries_a_node_cannot_receive() {
        let cases = [
            (
                "{ \"has-dash\": \"ok\" }",
                "`has-dash` is not a valid shell identifier",
            ),
            (
                "{ LD_PRELOAD: \"/tmp/evil.so\" }",
                "`LD_PRELOAD` is reserved",
            ),
            (
                "{ GREETING: \"hello world\" }",
                "value of env var `GREETING`",
            ),
            ("{ \"\": \"ok\" }", "not a valid shell identifier"),
        ];
        for (env_vars, expected) in cases {
            let json5 = format!("{{ instance_id: \"esp32_1\", env_vars: {env_vars} }}");
            let err = serde_json5::from_str::<DeploymentInstance>(&json5)
                .expect_err(&format!("`{env_vars}` must be rejected"));
            let msg = err.to_string();
            assert!(msg.contains(expected), "expected `{expected}`, got: {msg}");
        }
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
        assert!(!with_sim.framework.publishes_sim_time);

        let source: DeploymentInstance = serde_json5::from_str(
            "{ instance_id: \"sim_inst\", framework: { publishes_sim_time: true } }",
        )
        .unwrap();
        assert!(source.framework.publishes_sim_time);
        assert_eq!(source.framework.use_sim_time, None);
        let source_reparsed: DeploymentInstance =
            serde_json5::from_str(&serde_json5::to_string(&source).unwrap()).unwrap();
        assert!(source_reparsed.framework.publishes_sim_time);

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
                    source: { name: "left_camera:v1" },
                    instances: [{ instance_id: "cam_wrist_left", arguments: {} }]
                },
                {
                    source: { name: "right_camera:v1" },
                    instances: [{ instance_id: "cam_wrist_right", arguments: {} }]
                },
                {
                    source: { name: "torso_camera:v1" },
                    instances: [{ instance_id: "cam_torso", arguments: {} }]
                },
                {
                    source: { name: "backbone:v1" },
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
            Some(&LinkValue::Bound(Selection::Scalar(
                "cam_torso".to_string()
            )))
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
            Some(&LinkValue::Bound(Selection::Scalar(
                "camera_inst".to_string()
            )))
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
                    source: { name: "backbone:v1" },
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
            Some(&LinkValue::Bound(Selection::Scalar(
                "cam_torso".to_string()
            )))
        );
        assert_eq!(
            instance.links.get("b"),
            Some(&LinkValue::Bound(Selection::Scalar(
                "cam_torso".to_string()
            )))
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
                    source: { name: "backbone:v1" },
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

    /// An object link value is `{ vacant: "<why>" }` and nothing else. Every
    /// near miss is its own message, because "your object is wrong" would send
    /// the author looking in the wrong place.
    #[test]
    fn only_a_reasoned_vacancy_parses_as_an_object_link_value() {
        for (json5, expected) in [
            (r#"{ vacant: "" }"#, "a vacancy needs a reason"),
            (r#"{ vacant: "   " }"#, "a vacancy needs a reason"),
            (r#"{ vacant: 12 }"#, "invalid type"),
            (r#"{ vacant: null }"#, "invalid type"),
            (r#"{ vacant: ["a"] }"#, "invalid type"),
            (r#"{ vacant: { nested: 1 } }"#, "invalid type"),
            (r#"{}"#, "missing field `vacant`"),
            (r#"{ nope: "x" }"#, "unknown link key `nope`"),
            (r#"{ vacant: "x", extra: 1 }"#, "unknown link key `extra`"),
        ] {
            let err = serde_json5::from_str::<LinkValue>(json5)
                .expect_err(&format!("`{json5}` must not parse"));
            assert!(
                err.to_string().contains(expected),
                "`{json5}` should say `{expected}`, said: {err}"
            );
        }

        let value: LinkValue = serde_json5::from_str(r#"{ vacant: "  spaced out  " }"#)
            .expect("a reason that says something parses");
        assert_eq!(
            value.vacancy().map(VacantReason::as_str),
            Some("spaced out"),
            "surrounding whitespace is trimmed as the reason parses"
        );
    }

    /// A parsed vacancy serializes back to the shape it was written in, so a
    /// launcher that round-trips through the parser is still the same file.
    #[test]
    fn a_vacancy_round_trips_through_serialization() {
        let value: LinkValue =
            serde_json5::from_str(r#"{ vacant: "monitor rig: nothing commands this backbone" }"#)
                .expect("vacancy parses");
        let rendered = serde_json::to_string(&value).expect("vacancy serializes");
        assert_eq!(
            rendered,
            r#"{"vacant":"monitor rig: nothing commands this backbone"}"#
        );
        assert_eq!(
            serde_json::from_str::<LinkValue>(&rendered).expect("round trip parses"),
            value
        );
    }

    /// One slot, one value: a `links` map naming the same slot twice is
    /// rejected as it parses, which is what makes "linked AND vacant"
    /// unwritable rather than resolved by insertion order.
    #[test]
    fn a_links_map_cannot_name_one_slot_twice() {
        let err = serde_json5::from_str::<DeploymentInstance>(
            r#"{
                instance_id: "ctrl_1",
                links: { arm: "arm_1", arm: { vacant: "no arm here" } }
            }"#,
        )
        .expect_err("one slot cannot hold two values");
        assert!(
            err.to_string().contains("arm"),
            "the duplicate key should be named: {err}"
        );
    }

    /// Every `links` value shape parses, resolves against siblings, and
    /// supports the `/<peer_link_id>` disambiguation suffix.
    #[test]
    fn links_parse_every_value_shape() {
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
                        links: {
                            arm: "arm_1",
                            spare: { vacant: "bench rig: only one arm on this bench" },
                            watched: ["arm_1"]
                        }
                    }]
                }
            ]
        }"#;
        let launcher: PeppyLauncher = serde_json5::from_str(json5).expect("launcher should parse");
        let ctrl = &launcher.deployments[1].instances[0];
        assert_eq!(scalar_target(ctrl, "arm"), Some("arm_1"));
        assert_eq!(
            ctrl.links
                .get("spare")
                .and_then(LinkValue::vacancy)
                .map(VacantReason::as_str),
            Some("bench rig: only one arm on this bench")
        );
        assert_eq!(
            ctrl.links
                .get("watched")
                .and_then(LinkValue::selection)
                .map(Selection::targets),
            Some(["arm_1".to_string()].as_slice())
        );

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
        assert_eq!(scalar_target(ctrl, "arm"), Some("arm_1/controller"));
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

    fn core_node(name: &str) -> crate::core_node_name::CoreNodeName {
        crate::core_node_name::CoreNodeName::new(name).expect("valid test core node name")
    }

    /// The participants are the machines the given instances actually run
    /// on: the coordinator only when something defaulted to it, every placed
    /// machine once however many instances it holds, in a stable order.
    #[test]
    fn placements_participants_are_the_machines_the_instances_run_on() {
        let placements = Placements::new(
            core_node("cn-coord"),
            BTreeMap::from([
                ("robot_a".to_owned(), core_node("cn-b")),
                ("robot_b".to_owned(), core_node("cn-b")),
                ("robot_c".to_owned(), core_node("cn-a")),
            ]),
        );

        assert_eq!(
            placements.participants(["sim", "robot_a", "robot_b", "robot_c"]),
            BTreeSet::from(["cn-a", "cn-b", "cn-coord"])
        );
        assert_eq!(
            placements.participants(["robot_a", "robot_b"]),
            BTreeSet::from(["cn-b"]),
            "a coordinator hosting nothing is no participant"
        );
        assert_eq!(
            Placements::all_on(core_node("cn-solo")).participants(["sim", "robot"]),
            BTreeSet::from(["cn-solo"])
        );
        assert!(placements.participants([]).is_empty());
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

    /// The daemon the launch was sent to, which is where an instance that
    /// declared no `core_node` runs.
    pub fn coordinator(&self) -> &str {
        self.coordinator.as_str()
    }

    /// Whether any instance is placed off the coordinator, i.e. whether this
    /// launch actually spans machines.
    pub fn is_federated(&self) -> bool {
        self.by_instance
            .values()
            .any(|core_node| core_node != &self.coordinator)
    }

    /// Every core node at least one of `instance_ids` runs on, deduplicated
    /// and in a stable order: the machines a launch-wide fact (the clock's
    /// fan-out) has to reach. Derived from the instances rather than from the
    /// placement map alone, since an instance that named no `core_node` runs
    /// on the coordinator and a coordinator hosting nothing is no participant.
    pub fn participants<'a>(
        &'a self,
        instance_ids: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<&'a str> {
        instance_ids.into_iter().map(|id| self.of(id)).collect()
    }
}
