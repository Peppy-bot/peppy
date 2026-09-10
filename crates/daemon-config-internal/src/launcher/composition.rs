//! The composition grammar: the `components` axes a `launcher/v1` or
//! `launcher_fragment/v1` document declares, the option entries a
//! `deployments` list may carry beside its nodes, and the `adjustments` and
//! `constraints` both document kinds may state.
//!
//! Components declare what may run; deployments say what runs. A launcher
//! whose `components` fill once (`one`, `zero_or_one`) describes a FAMILY of
//! stacks whose members differ in which option fills each axis, selected by
//! the file's `deployments` and swapped at launch with `--with`. An axis
//! with cardinality `zero_or_more` runs as named copies, each listed under
//! `deployments` or added with `stack join`. A fragment declares its own
//! axes the same way, filled once per copy of it. Composition
//! ( [`super::compose`] ) turns a launcher plus a selection into the
//! ordinary flat document the rest of the pipeline consumes; this module is
//! the grammar and the checks that need no I/O and no selection.

use super::types::{Deployment, LinkValue};
use config::{AnyType, runtime::Name, schema::PeppySchema};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, SeqAccess, Visitor},
    ser::{SerializeMap, SerializeSeq},
};
use std::collections::{BTreeMap, HashSet};

/// The `deployments` keys an entry is read by, and so the names a component
/// cannot have: an entry with `source` deploys a node; `instances`, `with`,
/// `arguments` and `adjustments` belong to an option entry.
const RESERVED_COMPONENT_NAMES: [&str; 5] =
    ["source", "instances", "with", "arguments", "adjustments"];

/// Argument overrides an option entry or one of its copies writes, keyed by
/// the instance id written in the option's fragment and then by argument.
pub type ArgumentOverrides = BTreeMap<String, BTreeMap<String, AnyType>>;

/// How many selections of an axis a document allows. `one` and
/// `zero_or_one` are filled once, by a `deployments` entry or `--with`, and
/// their instances keep the ids they are written with. `zero_or_more` runs
/// as named copies, each minting its ids under its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentCardinality {
    #[default]
    One,
    ZeroOrOne,
    ZeroOrMore,
}

impl ComponentCardinality {
    pub fn is_one(&self) -> bool {
        *self == Self::One
    }

    pub fn allows_empty(self) -> bool {
        self != Self::One
    }

    /// Whether the axis runs as named copies.
    pub fn is_repeatable(self) -> bool {
        self == Self::ZeroOrMore
    }
}

/// One axis declaration in a `components` list.
///
/// The list is ORDERED, and the order is load-bearing: it fixes the order
/// fragments are collected in and therefore the order adjustments apply and
/// deployments appear, the same way `deployments` order fixes start order.
/// An axis names the `options` that fill it, bounded by its cardinality.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentAxis {
    /// The axis's name, unique within its document and spelled with the
    /// same identifier grammar as every other name in a peppy document. Used
    /// by `--with axis=option`, by `when` guards, and by the `deployments`
    /// entry that fills it.
    pub name: String,
    /// The alternatives that fill this axis, at least one. A value is an
    /// inline fragment object, a path relative to the declaring document's
    /// directory, or a list mixing both (merged in list order).
    pub options: BTreeMap<String, FragmentSpec>,
    #[serde(default, skip_serializing_if = "ComponentCardinality::is_one")]
    pub cardinality: ComponentCardinality,
    /// The axis's interface: every option must define all of these instance
    /// ids in its deployments, so a link or adjustment written against one is
    /// valid regardless of which option the operator picks. On a repeatable
    /// axis the ids are checked as written, before a copy's name prefixes
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<Name>,
}

impl ComponentAxis {
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl<'de> Deserialize<'de> for ComponentAxis {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The keys an axis accepts, plus the ones a reader may reach for
        /// from another document shape, each refused with its replacement.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Declaration {
            name: String,
            #[serde(default)]
            options: Option<BTreeMap<String, FragmentSpec>>,
            #[serde(default)]
            cardinality: ComponentCardinality,
            #[serde(default)]
            provides: Vec<Name>,
            #[serde(default, deserialize_with = "present")]
            default: Option<de::IgnoredAny>,
            #[serde(default, deserialize_with = "present")]
            optional: Option<de::IgnoredAny>,
            #[serde(default, deserialize_with = "present")]
            components: Option<de::IgnoredAny>,
        }

        let raw = Declaration::deserialize(deserializer)?;
        if raw.default.is_some() {
            return Err(de::Error::custom(format!(
                "axis `{}` declares `default`; the option a document starts with is a \
                 `deployments` entry, `{{ {}: \"<option>\" }}`, which `--with` swaps",
                raw.name, raw.name
            )));
        }
        if raw.optional.is_some() {
            return Err(de::Error::custom(format!(
                "axis `{}` declares `optional`; write `cardinality: \"zero_or_one\"` for an \
                 axis that may stay unfilled",
                raw.name
            )));
        }
        if raw.components.is_some() {
            return Err(de::Error::custom(format!(
                "axis `{}` declares `components`; an option's own axes are declared by its \
                 fragment, under that fragment's `components`",
                raw.name
            )));
        }
        let Some(options) = raw.options else {
            return Err(de::Error::missing_field("options"));
        };
        Ok(Self {
            name: raw.name,
            options,
            cardinality: raw.cardinality,
            provides: raw.provides,
        })
    }
}

fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<de::IgnoredAny>, D::Error> {
    de::IgnoredAny::deserialize(deserializer).map(Some)
}

/// One option's fragment set, in the order the option declared it: an inline
/// fragment, a path, or a list mixing both all parse into the same shape.
///
/// The two origins parse identically but live differently: an inline fragment
/// is part of the declaring document, while a path is resolved and read when
/// the composition is loaded ( [`super::compose`] ), never here.
#[derive(Debug, Clone, Default)]
pub struct FragmentSpec(pub Vec<FragmentPart>);

/// One piece of an option's body: a fragment written inline, or a path to a
/// `launcher_fragment/v1` file.
#[derive(Debug, Clone)]
pub enum FragmentPart {
    Inline(Fragment),
    File(String),
}

/// The body of one fragment: what selecting its option pulls in.
///
/// `deployments` uses the same grammar as the launcher's own, node entries
/// and option entries alike, `core_nodes` are unioned with the base's, and
/// `adjustments` reach instances the fragment does not own. A fragment
/// declares its own `components`, filled once per copy of it, and the
/// `constraints` among them; it cannot declare `peppy_schema` when inline,
/// since the file form wraps this body in [`LauncherFragment`].
#[derive(Debug, Clone, Default)]
pub struct Fragment {
    /// The node entries of `deployments`.
    pub deployments: Vec<Deployment>,
    /// The option entries of `deployments`: the options this fragment
    /// deploys on its own axes unless a copy's `with` says otherwise.
    pub option_deployments: Vec<OptionDeployment>,
    pub components: Vec<ComponentAxis>,
    pub constraints: Vec<SelectionConstraint>,
    pub adjustments: Vec<Adjustment>,
    pub core_nodes: Vec<String>,
}

/// The document shape a fragment body is read from and written to.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFragment {
    #[serde(default)]
    deployments: DeploymentEntries,
    #[serde(default)]
    components: Vec<ComponentAxis>,
    #[serde(default)]
    constraints: Vec<SelectionConstraint>,
    #[serde(default)]
    adjustments: Vec<Adjustment>,
    #[serde(default)]
    core_nodes: Vec<String>,
}

impl Fragment {
    fn from_raw<E: de::Error>(raw: RawFragment) -> Result<Self, E> {
        validate_axes(&raw.components, AxisScope::Fragment).map_err(E::custom)?;
        validate_option_deployments(&raw.deployments.options, &raw.components)
            .map_err(E::custom)?;
        validate_constraint_shapes(&raw.constraints, "this fragment").map_err(E::custom)?;
        for adjustment in &raw.adjustments {
            validate_adjustment(adjustment, "this fragment").map_err(E::custom)?;
        }
        Ok(Self {
            deployments: raw.deployments.nodes,
            option_deployments: raw.deployments.options,
            components: raw.components,
            constraints: raw.constraints,
            adjustments: raw.adjustments,
            core_nodes: raw.core_nodes,
        })
    }
}

impl<'de> Deserialize<'de> for Fragment {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Fragment::from_raw(RawFragment::deserialize(deserializer)?)
    }
}

impl Serialize for Fragment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if !self.deployments.is_empty() || !self.option_deployments.is_empty() {
            map.serialize_entry(
                "deployments",
                &DeploymentEntriesRef {
                    nodes: &self.deployments,
                    options: &self.option_deployments,
                },
            )?;
        }
        if !self.components.is_empty() {
            map.serialize_entry("components", &self.components)?;
        }
        if !self.constraints.is_empty() {
            map.serialize_entry("constraints", &self.constraints)?;
        }
        if !self.adjustments.is_empty() {
            map.serialize_entry("adjustments", &self.adjustments)?;
        }
        if !self.core_nodes.is_empty() {
            map.serialize_entry("core_nodes", &self.core_nodes)?;
        }
        map.end()
    }
}

/// A `launcher_fragment/v1` document: a [`Fragment`] body in its own file,
/// tagged so the repository indexer can tell a fragment from a launcher and
/// so a mis-tagged document refuses to parse as the wrong kind.
#[derive(Debug, Clone)]
pub struct LauncherFragment {
    pub peppy_schema: PeppySchema,
    pub body: Fragment,
}

impl<'de> Deserialize<'de> for LauncherFragment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The body's fields are declared here rather than `#[serde(flatten)]`ed
        // in: serde ignores `deny_unknown_fields` on both sides of a flatten,
        // so a misspelled field would be silently dropped instead of refused.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawLauncherFragment {
            #[serde(deserialize_with = "deserialize_launcher_fragment_v1_schema")]
            peppy_schema: PeppySchema,
            #[serde(default)]
            deployments: DeploymentEntries,
            #[serde(default)]
            components: Vec<ComponentAxis>,
            #[serde(default)]
            constraints: Vec<SelectionConstraint>,
            #[serde(default)]
            adjustments: Vec<Adjustment>,
            #[serde(default)]
            core_nodes: Vec<String>,
        }

        let raw = RawLauncherFragment::deserialize(deserializer)?;
        Ok(LauncherFragment {
            peppy_schema: raw.peppy_schema,
            body: Fragment::from_raw(RawFragment {
                deployments: raw.deployments,
                components: raw.components,
                constraints: raw.constraints,
                adjustments: raw.adjustments,
                core_nodes: raw.core_nodes,
            })?,
        })
    }
}

/// Reject any `peppy_schema` value other than `launcher_fragment/v1` so a
/// whole launcher (or a node document) that happens to share the fragment
/// body cannot slip through the fragment parser.
fn deserialize_launcher_fragment_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::LauncherFragmentV1)
}

/// One option entry of a `deployments` list, `{ <axis>: "<option>" }`: the
/// option the document deploys on that axis. On a `zero_or_more` axis the
/// entry lists the copies it runs as under `instances`; its own `with`,
/// `arguments` and `adjustments` apply to each of them, a copy's own
/// winning per axis and per argument and running after the entry's.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionDeployment {
    pub axis: String,
    pub option: String,
    pub with: BTreeMap<String, String>,
    pub arguments: ArgumentOverrides,
    pub adjustments: Vec<Adjustment>,
    pub instances: Vec<CopyEntry>,
}

/// One adjustment with the origin the report names it by.
#[derive(Clone)]
pub(crate) struct OriginatedAdjustment<'a> {
    pub adjustment: &'a Adjustment,
    pub origin: String,
}

/// What one copy the file deploys selects and writes: the entry's settings
/// with the copy's own on top.
pub(crate) struct CopySettings<'a> {
    pub with: BTreeMap<String, String>,
    pub arguments: ArgumentOverrides,
    /// The entry's adjustments, then the copy's.
    pub adjustments: Vec<OriginatedAdjustment<'a>>,
}

/// The shape of a copy's or an option entry's `with` and `arguments`:
/// named axes and options, named instances, at least one argument each.
fn check_copy_settings(
    with: &BTreeMap<String, String>,
    arguments: &ArgumentOverrides,
    origin: &str,
) -> Result<(), String> {
    for (axis, option) in with {
        if axis.trim().is_empty() || option.trim().is_empty() {
            return Err(format!(
                "{origin} selects with an empty axis or option name"
            ));
        }
    }
    for (target, values) in arguments {
        if target.trim().is_empty() {
            return Err(format!(
                "{origin} overrides arguments of an instance with an empty id"
            ));
        }
        if values.is_empty() {
            return Err(format!(
                "{origin} overrides no argument of `{target}`; name at least one"
            ));
        }
    }
    Ok(())
}

impl OptionDeployment {
    /// Whether the entry carries `with`, `arguments` or `adjustments` for
    /// the copies it lists.
    pub fn has_copy_settings(&self) -> bool {
        !self.with.is_empty() || !self.arguments.is_empty() || !self.adjustments.is_empty()
    }

    /// The origin the report names this entry's adjustments by.
    pub(crate) fn adjustments_origin(&self) -> String {
        format!("adjustments of `{}: {}`", self.axis, self.option)
    }

    pub(crate) fn settings_for<'a>(&'a self, copy: &'a CopyEntry) -> CopySettings<'a> {
        let mut with = self.with.clone();
        with.extend(copy.with.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut arguments = self.arguments.clone();
        for (target, values) in &copy.arguments {
            arguments
                .entry(target.clone())
                .or_default()
                .extend(values.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let adjustments = self
            .adjustments
            .iter()
            .map(|adjustment| OriginatedAdjustment {
                adjustment,
                origin: self.adjustments_origin(),
            })
            .chain(
                copy.adjustments
                    .iter()
                    .map(|adjustment| OriginatedAdjustment {
                        adjustment,
                        origin: copy.adjustments_origin(),
                    }),
            )
            .collect();
        CopySettings {
            with,
            arguments,
            adjustments,
        }
    }
}

/// One named copy of a `zero_or_more` axis's option: `instance_id` prefixes
/// every id the option's fragments define (`alpha_backbone_inst`), `with`
/// selects the option's own axes, `arguments` overrides the arguments of
/// the option's instances, keyed by the ids written in the fragment, and
/// `adjustments` write to those instances with the adjustment verbs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CopyEntry {
    pub instance_id: Name,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub with: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arguments: ArgumentOverrides,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adjustments: Vec<Adjustment>,
}

impl CopyEntry {
    /// The origin the report names this copy's adjustments by.
    pub(crate) fn adjustments_origin(&self) -> String {
        format!("adjustments of copy `{}`", self.instance_id)
    }
}

impl<'de> Deserialize<'de> for CopyEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Declaration {
            instance_id: Name,
            #[serde(default)]
            with: BTreeMap<String, String>,
            #[serde(default)]
            arguments: ArgumentOverrides,
            #[serde(default)]
            adjustments: Vec<Adjustment>,
            #[serde(default, deserialize_with = "present")]
            core_node: Option<de::IgnoredAny>,
            #[serde(default, deserialize_with = "present")]
            links: Option<de::IgnoredAny>,
        }

        let raw = Declaration::deserialize(deserializer)?;
        for adjustment in &raw.adjustments {
            validate_adjustment(adjustment, &format!("copy `{}`", raw.instance_id))
                .map_err(de::Error::custom)?;
        }
        if raw.core_node.is_some() {
            return Err(de::Error::custom(format!(
                "copy `{}` declares `core_node`; a copy is placed as a whole with \
                 `--place {}@CORE_NODE`, its name being its placement link",
                raw.instance_id, raw.instance_id
            )));
        }
        if raw.links.is_some() {
            return Err(de::Error::custom(format!(
                "copy `{}` declares `links`; a copy's wiring is written in its option's \
                 fragment, and `with` selects the fragment's own axes",
                raw.instance_id
            )));
        }
        check_copy_settings(
            &raw.with,
            &raw.arguments,
            &format!("copy `{}`", raw.instance_id),
        )
        .map_err(de::Error::custom)?;
        Ok(Self {
            instance_id: raw.instance_id,
            with: raw.with,
            arguments: raw.arguments,
            adjustments: raw.adjustments,
        })
    }
}

/// A `deployments` list, read into its node entries and its option entries.
/// An entry with a `source` key deploys a node; otherwise its one key beside
/// `instances` names a component and the value that component's option.
#[derive(Debug, Clone, Default)]
pub(crate) struct DeploymentEntries {
    pub(crate) nodes: Vec<Deployment>,
    pub(crate) options: Vec<OptionDeployment>,
}

impl<'de> Deserialize<'de> for DeploymentEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = DeploymentEntries;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a list of deployments: `{ source: { name, tag }, instances: [...] }` for \
                     a node, `{ <component>: \"<option>\" }` for a component's option",
                )
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut entries = DeploymentEntries::default();
                let mut index = 0usize;
                while let Some(entry) =
                    seq.next_element::<serde_json::Map<String, serde_json::Value>>()?
                {
                    match read_deployment_entry(entry).map_err(|refusal| match refusal {
                        // A node entry's own refusals carry their structure
                        // and their field path; an option entry's name the
                        // entry by position.
                        EntryRefusal::Node(message) => de::Error::custom(message),
                        EntryRefusal::Option(reason) => {
                            de::Error::custom(format!("deployments[{index}]: {reason}"))
                        }
                    })? {
                        DeploymentEntry::Node(deployment) => entries.nodes.push(deployment),
                        DeploymentEntry::Option(option) => entries.options.push(option),
                    }
                    index += 1;
                }
                Ok(entries)
            }
        }

        deserializer.deserialize_seq(EntriesVisitor)
    }
}

enum DeploymentEntry {
    Node(Deployment),
    Option(OptionDeployment),
}

/// Why one `deployments` entry was refused: a node entry's message, kept
/// as its own parser wrote it, or an option entry's reason.
enum EntryRefusal {
    Node(String),
    Option(String),
}

/// Reads one `deployments` entry by its keys.
fn read_deployment_entry(
    entry: serde_json::Map<String, serde_json::Value>,
) -> Result<DeploymentEntry, EntryRefusal> {
    if entry.contains_key("source") {
        let deployment = Deployment::deserialize(serde_json::Value::Object(entry))
            .map_err(|e| EntryRefusal::Node(e.to_string()))?;
        return Ok(DeploymentEntry::Node(deployment));
    }
    if ["name", "tag", "exposures"]
        .iter()
        .any(|key| entry.contains_key(*key))
    {
        return Err(EntryRefusal::Option(
            "a node deployment names what it runs under `source`: `{ source: { name, tag }, \
             instances: [...] }`"
                .to_owned(),
        ));
    }
    let mut keys = entry
        .keys()
        .filter(|key| !RESERVED_COMPONENT_NAMES.contains(&key.as_str()));
    let Some(axis) = keys.next().cloned() else {
        return Err(EntryRefusal::Option(
            "an entry deploys a node (`{ source: { name, tag }, instances: [...] }`) or a \
             component's option (`{ <component>: \"<option>\" }`); this one names neither"
                .to_owned(),
        ));
    };
    if let Some(second) = keys.next() {
        return Err(EntryRefusal::Option(format!(
            "an entry deploys one component's option; this one names `{axis}` and `{second}`"
        )));
    }
    let option = match &entry[&axis] {
        serde_json::Value::String(option) => option.clone(),
        other => {
            return Err(EntryRefusal::Option(format!(
                "`{axis}` must name one of its options as a string, e.g. `{{ {axis}: \
                 \"<option>\" }}`; found {other}"
            )));
        }
    };
    if option.trim().is_empty() {
        return Err(EntryRefusal::Option(format!(
            "`{axis}` names an empty option"
        )));
    }
    let instances = match entry.get("instances") {
        None => Vec::new(),
        Some(value) => Vec::<CopyEntry>::deserialize(value.clone())
            .map_err(|e| EntryRefusal::Option(format!("`instances` of `{axis}: {option}`: {e}")))?,
    };
    let with = match entry.get("with") {
        None => BTreeMap::new(),
        Some(value) => BTreeMap::<String, String>::deserialize(value.clone())
            .map_err(|e| EntryRefusal::Option(format!("`with` of `{axis}: {option}`: {e}")))?,
    };
    let arguments = match entry.get("arguments") {
        None => BTreeMap::new(),
        Some(value) => BTreeMap::<String, BTreeMap<String, AnyType>>::deserialize(value.clone())
            .map_err(|e| EntryRefusal::Option(format!("`arguments` of `{axis}: {option}`: {e}")))?,
    };
    let adjustments = match entry.get("adjustments") {
        None => Vec::new(),
        Some(value) => Vec::<Adjustment>::deserialize(value.clone()).map_err(|e| {
            EntryRefusal::Option(format!("`adjustments` of `{axis}: {option}`: {e}"))
        })?,
    };
    for adjustment in &adjustments {
        validate_adjustment(adjustment, &format!("`{axis}: {option}`"))
            .map_err(EntryRefusal::Option)?;
    }
    check_copy_settings(&with, &arguments, &format!("`{axis}: {option}`"))
        .map_err(EntryRefusal::Option)?;
    Ok(DeploymentEntry::Option(OptionDeployment {
        axis,
        option,
        with,
        arguments,
        adjustments,
        instances,
    }))
}

/// A `deployments` list written back in document order: node entries first,
/// then the option entries.
pub(crate) struct DeploymentEntriesRef<'a> {
    pub(crate) nodes: &'a [Deployment],
    pub(crate) options: &'a [OptionDeployment],
}

impl Serialize for DeploymentEntriesRef<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.nodes.len() + self.options.len()))?;
        for node in self.nodes {
            seq.serialize_element(node)?;
        }
        for option in self.options {
            seq.serialize_element(option)?;
        }
        seq.end()
    }
}

impl Serialize for OptionDeployment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry(&self.axis, &self.option)?;
        if !self.with.is_empty() {
            map.serialize_entry("with", &self.with)?;
        }
        if !self.arguments.is_empty() {
            map.serialize_entry("arguments", &self.arguments)?;
        }
        if !self.adjustments.is_empty() {
            map.serialize_entry("adjustments", &self.adjustments)?;
        }
        if !self.instances.is_empty() {
            map.serialize_entry("instances", &self.instances)?;
        }
        map.end()
    }
}

/// An axis matches any of these distinct option names. Construction requires at least one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionOptions(Vec<Name>);

impl ConditionOptions {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(Name::as_str)
    }

    pub fn contains(&self, option: &str) -> bool {
        self.iter().any(|name| name == option)
    }
}

impl<'de> Deserialize<'de> for ConditionOptions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Options {
            One(Name),
            AnyOf(Vec<Name>),
        }

        let mut names = match Options::deserialize(deserializer)? {
            Options::One(name) => vec![name],
            Options::AnyOf(names) => names,
        };
        if names.is_empty() {
            return Err(de::Error::custom(
                "a condition must name at least one option",
            ));
        }
        names.sort();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(de::Error::custom("a condition must name each option once"));
        }
        Ok(Self(names))
    }
}

impl Serialize for ConditionOptions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [name] => name.serialize(serializer),
            names => names.serialize(serializer),
        }
    }
}

impl std::fmt::Display for ConditionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_slice() {
            [name] => write!(f, "{name}"),
            _ => write!(f, "[{}]", self.iter().collect::<Vec<_>>().join(", ")),
        }
    }
}

/// All named axes must match; each axis accepts one or several option names.
pub type SelectionCondition = BTreeMap<String, ConditionOptions>;

/// One rule about which selections a document refuses to be: `when` a
/// guard holds, the selection must also satisfy at least one `requires`
/// alternative and match no `forbids` entry, or the whole selection is
/// refused before anything is pinned or started.
///
/// Constraints exist for the members of a family that FLATTEN cleanly into a
/// stack nobody should run: a `when` guard on an adjustment can only decide
/// whether that adjustment applies inside a legal selection, and `provides`
/// only promises ids exist. Neither can say "this option needs another axis
/// filled". A constraint can, and it only ever refuses: it never picks an
/// option to satisfy itself, because a `--with` whose meaning shifts as
/// constraints evolve would launch stacks the operator did not name.
///
/// A launcher's constraints speak in the launcher's axis and option names; a
/// fragment's speak in its own axes and the launcher's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionConstraint {
    /// Guard: the selections this constraint speaks about, in the same
    /// `axis: options` grammar as an adjustment's `when` (several axes are an
    /// AND, several options on one axis an OR). Absent means every selection;
    /// an unfilled axis matches no entry, so a constraint guarded on its
    /// option stays quiet when the axis is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<SelectionCondition>,
    /// The alternatives, at least one of which the selection must satisfy
    /// wholly: each is an `axis: options` map (an AND), and listing several is
    /// an OR. An unfilled axis satisfies no entry, which is exactly
    /// how "this option needs a consumer" is written: require the axes that
    /// can consume it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<SelectionCondition>,
    /// The combinations the guarded selections must not contain: each entry
    /// is an `axis: options` map (an AND), and matching any entry refuses the
    /// selection. The direct form of "these options never launch together",
    /// which `requires` cannot say about an OPTIONAL axis's option (nothing
    /// requirable means "that axis stays off"). Where the axis is required,
    /// prefer requiring the wanted option instead: `requires` fails closed
    /// for options added later, while a `forbids` list names today's options
    /// and stays silent about tomorrow's.
    ///
    /// A constraint states at least one of `requires` and `forbids`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbids: Vec<SelectionCondition>,
    /// Why the refused selections must not launch, quoted verbatim in the
    /// refusal. Required, like a vacancy's reason: the engine can render
    /// what was required, but only the author knows what the dead
    /// combination would have done.
    pub reason: String,
}

/// One change to an instance defined elsewhere, in the base or in another
/// selected fragment. Every change names its operation; nothing is inferred
/// from a value's JSON type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adjustment {
    /// The instance to change. Whether an instance exists is declared by the
    /// base and the axes' options, so an adjustment whose target the resolved
    /// selection does not define is skipped, not refused; a target no option
    /// of the launcher defines anywhere is a dead reference and a parse error.
    pub target: Name,
    /// Guard: every named axis must hold one of its named options for this
    /// adjustment to run. Absent means unconditional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<SelectionCondition>,
    /// Replaces the target's value for each named top-level argument key,
    /// creating it when absent. Nested objects replace wholesale; the node's
    /// parameter schema validates the final value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_arguments: Option<BTreeMap<String, AnyType>>,
    /// Replaces the target's whole entry for each named slot, creating it
    /// when absent, using the ordinary launcher link grammar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_links: Option<BTreeMap<String, LinkValue>>,
    /// Appends targets to each named slot's array binding, creating the
    /// binding when the slot has no entry. Refused (at flatten time) when the
    /// slot holds a scalar or a vacancy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_links: Option<BTreeMap<String, Vec<String>>>,
    /// Drops each named slot's entry entirely, returning the slot to absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unset_links: Option<Vec<String>>,
}

impl Serialize for FragmentSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0.as_slice() {
            [single] => single.serialize(serializer),
            parts => {
                let mut seq = serializer.serialize_seq(Some(parts.len()))?;
                for part in parts {
                    seq.serialize_element(part)?;
                }
                seq.end()
            }
        }
    }
}

/// One fragment part read from a path string: the fragment lives in its
/// own `launcher_fragment/v1` file next to the declaring document.
fn fragment_part_from_str<E: de::Error>(v: &str) -> Result<FragmentPart, E> {
    if v.trim().is_empty() {
        return Err(de::Error::custom(
            "a fragment path cannot be empty: name a `launcher_fragment/v1` file relative to \
             the declaring document's directory",
        ));
    }
    Ok(FragmentPart::File(v.to_owned()))
}

/// One fragment part read from a map: an inline fragment body, written
/// where the option is.
fn fragment_part_from_map<'de, A: MapAccess<'de>>(map: A) -> Result<FragmentPart, A::Error> {
    let fragment = Fragment::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
    Ok(FragmentPart::Inline(fragment))
}

impl<'de> Deserialize<'de> for FragmentSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FragmentSpecVisitor;

        impl<'de> Visitor<'de> for FragmentSpecVisitor {
            type Value = FragmentSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a fragment object, a path string, or a list mixing both, e.g. \
                     [\"fragments/sim_relays.json5\", { deployments: [...] }]",
                )
            }

            /// A path string or an inline fragment: one part, read the same
            /// way a lone [`FragmentPart`] is.
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FragmentSpec(vec![fragment_part_from_str(v)?]))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                Ok(FragmentSpec(vec![fragment_part_from_map(map)?]))
            }

            /// The list form: parts merge in list order before the option's
            /// contribution enters composition.
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut parts = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(part) = seq.next_element::<FragmentPart>()? {
                    parts.push(part);
                }
                if parts.is_empty() {
                    return Err(de::Error::custom(
                        "a fragment list cannot be empty: an option with no body has nothing to \
                         contribute; an off-by-default feature is an axis with cardinality \
                         `zero_or_one`",
                    ));
                }
                Ok(FragmentSpec(parts))
            }
        }

        deserializer.deserialize_any(FragmentSpecVisitor)
    }
}

impl<'de> Deserialize<'de> for FragmentPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FragmentPartVisitor;

        impl<'de> Visitor<'de> for FragmentPartVisitor {
            type Value = FragmentPart;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a fragment object or a path string")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                fragment_part_from_str(v)
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                fragment_part_from_map(map)
            }
        }

        deserializer.deserialize_any(FragmentPartVisitor)
    }
}

impl Serialize for FragmentPart {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            FragmentPart::Inline(fragment) => fragment.serialize(serializer),
            FragmentPart::File(path) => serializer.serialize_str(path),
        }
    }
}

/// Parser for `launcher_fragment/v1` documents.
///
/// Fragment files have no fixed name: any `.json5` file whose body declares
/// the fragment schema is one. A fragment is inert on its own; it only ever
/// contributes to the document whose option references it.
pub struct LauncherFragmentParser;

impl LauncherFragmentParser {
    pub fn from_path(file: impl AsRef<std::path::Path>) -> crate::error::Result<LauncherFragment> {
        let content = crate::parsing::read_non_empty_file(file.as_ref())?;
        Self::from_content(&content)
    }

    pub fn from_content(content: &str) -> crate::error::Result<LauncherFragment> {
        crate::error::deserialize_json5_with_path(content)
    }
}

/// Axis and option names share the identifier grammar of every other name in
/// a peppy document (the one [`Name`] enforces) and for the same reason: an
/// axis name is one half of `--with axis=option` and a bare `--with` word is
/// an option name, so a name carrying an `=`, a `,`, or whitespace would be
/// unwireable from the very surface it exists for. (`,` separates `--with`
/// entries and `=` splits the `axis=option` form; both are outside the
/// identifier grammar.)
pub(crate) fn check_axis_or_option_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("`components` declares {kind} with an empty name"));
    }
    if Name::try_from(name.to_owned()).is_err() {
        return Err(format!(
            "`components` declares {kind} `{name}`, which is not a plain identifier \
             (letters, digits, `_`, `-`): the name appears in `--with` entries and `when` \
             guards, where `=` and `,` already carry meaning and whitespace separates words"
        ));
    }
    Ok(())
}

/// Which document declares a `components` list: the rules differ in what a
/// fragment's axes may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxisScope {
    Launcher,
    Fragment,
}

/// The document-local checks on a `components` list: the ones needing no
/// fragment file and no selection. Runs while the document parses, so an
/// author hears them from any reader of the file.
///
/// What it deliberately does NOT check: that an option defines its axis's
/// `provides` ids and that `when` guards name real axes and options, both of
/// which need the fragments loaded ( [`super::compose`] ).
pub(crate) fn validate_axes(axes: &[ComponentAxis], scope: AxisScope) -> Result<(), String> {
    let mut axis_names = HashSet::with_capacity(axes.len());
    for axis in axes {
        check_axis_or_option_name("an axis", &axis.name)?;
        if RESERVED_COMPONENT_NAMES.contains(&axis.name.as_str()) {
            return Err(format!(
                "`{}` cannot be a component name: a `deployments` entry with a `source` key \
                 deploys a node, and `instances`, `with`, `arguments` and `adjustments` belong \
                 to an option entry",
                axis.name
            ));
        }
        if !axis_names.insert(axis.as_str()) {
            return Err(format!(
                "`components` declares the axis `{}` more than once; axis names must be unique",
                axis.name
            ));
        }
        if axis.options.is_empty() {
            return Err(format!(
                "axis `{}` declares no `options`; an axis with nothing to choose between is \
                 not a component. State its alternatives, or drop the axis",
                axis.name
            ));
        }
        for option in axis.options.keys() {
            check_axis_or_option_name(&format!("an option of axis `{}`", axis.name), option)?;
        }
        if scope == AxisScope::Fragment && axis.cardinality.is_repeatable() {
            return Err(format!(
                "axis `{}` declares `zero_or_more` inside a fragment; copies are the \
                 launcher's to deploy, so declare this axis in the launcher, or give it \
                 `one` or `zero_or_one`",
                axis.name
            ));
        }
    }

    // A bare `--with` word resolves against option names, so an axis name
    // sharing a name with another axis's option would make that word name two
    // different things. An axis and its OWN option may share a name: a
    // single-option `zero_or_one` axis is a feature toggle, and both readings of
    // the word select the same option.
    for axis in axes {
        for other in axes {
            if other.name == axis.name {
                continue;
            }
            if other.options.contains_key(axis.name.as_str()) {
                return Err(format!(
                    "axis `{}` cannot share a name with the `{}` option of axis `{}`: a bare \
                     `--with {}` would not say which of the two it selects",
                    axis.name, axis.name, other.name, axis.name
                ));
            }
        }
    }
    // `peppy stack join OPTION` names a copy by its option alone, so the
    // options of the axes that run as copies are distinct across those axes.
    let mut copy_options: BTreeMap<&str, &str> = BTreeMap::new();
    for axis in axes.iter().filter(|axis| axis.cardinality.is_repeatable()) {
        for option in axis.options.keys() {
            if let Some(first) = copy_options.insert(option, &axis.name) {
                return Err(format!(
                    "`{option}` is an option of both `{first}` and `{}`, which run as copies; \
                     `peppy stack join {option}` would not say which one, so give one of them \
                     another name",
                    axis.name
                ));
            }
        }
    }
    Ok(())
}

/// The document-local checks on the option entries of a `deployments` list:
/// each names a declared axis and one of its options, deploys it the way
/// its cardinality allows, and names its copies once.
pub(crate) fn validate_option_deployments(
    entries: &[OptionDeployment],
    axes: &[ComponentAxis],
) -> Result<(), String> {
    let mut deployed_axes: BTreeMap<&str, &str> = BTreeMap::new();
    let mut copies: HashSet<&str> = HashSet::new();
    for entry in entries {
        let Some(axis) = axes.iter().find(|axis| axis.name == entry.axis) else {
            return Err(format!(
                "`deployments` deploys `{}: \"{}\"`, but this document declares no axis \
                 `{}`. Axes: {}",
                entry.axis,
                entry.option,
                entry.axis,
                crate::error::format_quoted_list(axes.iter().map(ComponentAxis::as_str))
            ));
        };
        if !axis.options.contains_key(&entry.option) {
            return Err(format!(
                "`deployments` deploys `{}: \"{}\"`, which axis `{}` does not declare. Its \
                 options: {}",
                entry.axis,
                entry.option,
                entry.axis,
                crate::error::format_quoted_list(axis.options.keys())
            ));
        }
        match axis.cardinality {
            ComponentCardinality::One => {
                if !entry.instances.is_empty() || entry.has_copy_settings() {
                    return Err(format!(
                        "axis `{}` has cardinality `one`: its option runs once, ids as \
                         written, so `{{ {}: \"{}\" }}` takes no `instances`, `with`, \
                         `arguments` or `adjustments`; `--with` selects its axes at launch \
                         and the launcher's `adjustments` write to its instances",
                        entry.axis, entry.axis, entry.option
                    ));
                }
                if let Some(first) = deployed_axes.insert(&entry.axis, &entry.option) {
                    return Err(format!(
                        "`deployments` deploys axis `{}` twice (`{first}`, `{}`); a `one` \
                         axis deploys one option",
                        entry.axis, entry.option
                    ));
                }
            }
            ComponentCardinality::ZeroOrOne => {
                return Err(format!(
                    "axis `{}` has cardinality `zero_or_one` and cannot be deployed by the \
                     file, since nothing on the command line could switch it off. Select it \
                     at launch with `--with {}`, or declare the axis `one`",
                    entry.axis, entry.option
                ));
            }
            ComponentCardinality::ZeroOrMore => {
                if entry.instances.is_empty() {
                    return Err(format!(
                        "axis `{}` runs as named copies: `{{ {}: \"{}\", instances: [{{ \
                         instance_id: \"alpha\" }}] }}`",
                        entry.axis, entry.axis, entry.option
                    ));
                }
                for copy in &entry.instances {
                    if !copies.insert(copy.instance_id.as_str()) {
                        return Err(format!(
                            "`deployments` names copy `{}` twice; each copy has its own name",
                            copy.instance_id
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// The shape checks on one adjustment: names that say something, operations
/// that exist, targets that are well-formed. Selection-independent, so they
/// run wherever the adjustment is read (document parse for the base and
/// inline fragments, fragment load for files).
pub(crate) fn validate_adjustment(adjustment: &Adjustment, origin: &str) -> Result<(), String> {
    if let Some(when) = &adjustment.when
        && when.is_empty()
    {
        return Err(format!(
            "adjustment on `{}` in {origin} declares an empty `when`: a guard names at least \
             one axis, or omits `when` entirely",
            adjustment.target
        ));
    }

    let has_operation = adjustment
        .set_arguments
        .as_ref()
        .is_some_and(|m| !m.is_empty())
        || adjustment.set_links.as_ref().is_some_and(|m| !m.is_empty())
        || adjustment.add_links.as_ref().is_some_and(|m| !m.is_empty())
        || adjustment
            .unset_links
            .as_ref()
            .is_some_and(|v| !v.is_empty());
    if !has_operation {
        return Err(format!(
            "adjustment on `{}` in {origin} names no operation: state at least one of \
             `set_arguments`, `set_links`, `add_links`, `unset_links`",
            adjustment.target
        ));
    }

    if let Some(arguments) = &adjustment.set_arguments {
        check_non_empty_keys(arguments.keys(), "argument", &adjustment.target, origin)?;
    }
    if let Some(links) = &adjustment.set_links {
        check_non_empty_keys(links.keys(), "link", &adjustment.target, origin)?;
    }
    if let Some(links) = &adjustment.add_links {
        check_non_empty_keys(links.keys(), "link", &adjustment.target, origin)?;
        for (slot, targets) in links {
            if targets.is_empty() {
                return Err(format!(
                    "adjustment on `{}` in {origin} adds no target to slot `{slot}`: an empty \
                     `add_links` entry appends nothing",
                    adjustment.target
                ));
            }
            let mut seen = HashSet::with_capacity(targets.len());
            for target in targets {
                if target.trim().is_empty() {
                    return Err(format!(
                        "adjustment on `{}` in {origin} adds an empty target to slot `{slot}`",
                        adjustment.target
                    ));
                }
                let (instance, link_suffix) = super::types::split_link_target(target);
                if instance.is_empty()
                    || link_suffix.is_some_and(|l| l.is_empty() || l.contains('/'))
                {
                    return Err(format!(
                        "adjustment on `{}` in {origin} adds target `{target}` to slot \
                         `{slot}`, which is malformed: expected `<instance>` or \
                         `<instance>/<link_id>`",
                        adjustment.target
                    ));
                }
                if !seen.insert(target.as_str()) {
                    return Err(format!(
                        "adjustment on `{}` in {origin} adds target `{target}` to slot \
                         `{slot}` more than once: a slot's bound set lists each producer once",
                        adjustment.target
                    ));
                }
            }
        }
    }
    if let Some(slots) = &adjustment.unset_links {
        let mut seen = HashSet::with_capacity(slots.len());
        for slot in slots {
            if slot.trim().is_empty() {
                return Err(format!(
                    "adjustment on `{}` in {origin} unsets an empty link key",
                    adjustment.target
                ));
            }
            if !seen.insert(slot.as_str()) {
                return Err(format!(
                    "adjustment on `{}` in {origin} unsets slot `{slot}` more than once",
                    adjustment.target
                ));
            }
        }
    }
    Ok(())
}

/// The keys an adjustment names must say something, whatever map they arrive
/// in. Duplicates inside one JSON5 object are collapsed by the map itself,
/// exactly as the per-instance `arguments` and `links` maps are.
fn check_non_empty_keys<'a>(
    keys: impl Iterator<Item = &'a String>,
    kind: &str,
    target: &Name,
    origin: &str,
) -> Result<(), String> {
    for key in keys {
        if key.trim().is_empty() {
            return Err(format!(
                "adjustment on `{target}` in {origin} names an empty {kind} key"
            ));
        }
    }
    Ok(())
}

/// That a `when` guard names axes `axes` declares and options those axes
/// declare. A guard is what licenses an adjustment to depend on the shape
/// those options define, so naming something else is a dead reference.
pub(crate) fn validate_guard(
    when: &SelectionCondition,
    axes: &[&ComponentAxis],
    origin: &str,
) -> Result<(), String> {
    validate_selection_map(when, axes, "a `when` guard", origin)
}

/// The shared half of guard and constraint validation: every `axis: options`
/// entry names an axis in `axes` and options that axis declares. `what`
/// names the map's role in the error (a `when` guard, a `requires`
/// alternative), which is all that differs between them.
fn validate_selection_map(
    map: &SelectionCondition,
    axes: &[&ComponentAxis],
    what: &str,
    origin: &str,
) -> Result<(), String> {
    for (axis_name, options) in map {
        let Some(axis) = axes.iter().find(|axis| axis.name == *axis_name) else {
            return Err(format!(
                "{what} in {origin} names axis `{axis_name}`, which is not in reach. Axes: {}",
                crate::error::format_quoted_list(axes.iter().map(|axis| axis.as_str()))
            ));
        };
        if let Some(option_name) = options
            .iter()
            .find(|option| !axis.options.contains_key(*option))
        {
            return Err(format!(
                "{what} in {origin} names option `{option_name}` on axis \
                 `{axis_name}`, which the axis does not declare. Its options: {}",
                crate::error::format_quoted_list(axis.options.keys())
            ));
        }
    }
    Ok(())
}

/// The shape checks on a `constraints` list: entries that say something and
/// lists that agree with themselves. Reference checks (that the axes and
/// options named exist) are [`validate_constraint_references`]'s, run once
/// the axes in reach are known.
pub(crate) fn validate_constraint_shapes(
    constraints: &[SelectionConstraint],
    document: &str,
) -> Result<(), String> {
    for (index, constraint) in constraints.iter().enumerate() {
        let origin = constraint_origin(document, index);
        if let Some(when) = &constraint.when
            && when.is_empty()
        {
            return Err(format!(
                "{origin} declares an empty `when`: a guard names at least one \
                 axis, or omits `when` entirely to speak about every \
                 selection"
            ));
        }
        if constraint.requires.is_empty() && constraint.forbids.is_empty() {
            return Err(format!(
                "{origin} declares neither `requires` nor `forbids`: a constraint states what \
                 the guarded selections must also have, or must not combine with, and with \
                 neither it refuses nothing"
            ));
        }
        for (entries, kind, verb) in [
            (&constraint.requires, "`requires` alternative", "requires"),
            (&constraint.forbids, "`forbids` entry", "forbids"),
        ] {
            let mut seen: Vec<&SelectionCondition> = Vec::with_capacity(entries.len());
            for entry in entries {
                if entry.is_empty() {
                    return Err(format!(
                        "{origin} lists an empty {kind}: an entry names at least one axis"
                    ));
                }
                if let Some(when) = &constraint.when {
                    for axis_name in entry.keys() {
                        if when
                            .get(axis_name)
                            .is_some_and(|options| options.0.len() == 1)
                        {
                            return Err(format!(
                                "{origin} {verb} axis `{axis_name}` inside a constraint whose \
                                 `when` already fixes that axis: under the guard the entry is \
                                 decided before anything is selected. Restate it without the \
                                 axis"
                            ));
                        }
                    }
                }
                if seen.contains(&entry) {
                    return Err(format!("{origin} lists the same {kind} more than once"));
                }
                seen.push(entry);
            }
        }
        for alternative in &constraint.requires {
            if constraint.forbids.contains(alternative) {
                return Err(format!(
                    "{origin} lists the same `axis: options` map in `requires` and `forbids`: \
                     a combination cannot be both what satisfies the constraint and what it \
                     refuses"
                ));
            }
        }
        if constraint.reason.trim().is_empty() {
            return Err(format!(
                "{origin} has an empty `reason`: the refusal quotes it to the operator, and \
                 only the author knows why the refused combinations must not launch"
            ));
        }
    }
    Ok(())
}

/// That every axis and option a `constraints` list names is in reach.
pub(crate) fn validate_constraint_references(
    constraints: &[SelectionConstraint],
    axes: &[&ComponentAxis],
    document: &str,
) -> Result<(), String> {
    for (index, constraint) in constraints.iter().enumerate() {
        let origin = constraint_origin(document, index);
        if let Some(when) = &constraint.when {
            validate_selection_map(when, axes, "a `when` guard", &origin)?;
        }
        for entry in &constraint.requires {
            validate_selection_map(entry, axes, "a `requires` alternative", &origin)?;
        }
        for entry in &constraint.forbids {
            validate_selection_map(entry, axes, "a `forbids` entry", &origin)?;
        }
    }
    Ok(())
}

fn constraint_origin(document: &str, index: usize) -> String {
    format!("{document}'s `constraints` entry {}", index + 1)
}

/// The document-local checks on a launcher's `constraints`: shapes, and
/// references against the launcher's own axes.
pub(crate) fn validate_launcher_constraints(
    constraints: &[SelectionConstraint],
    axes: &[ComponentAxis],
) -> Result<(), String> {
    validate_constraint_shapes(constraints, "the launcher")?;
    let in_reach: Vec<&ComponentAxis> = axes.iter().collect();
    validate_constraint_references(constraints, &in_reach, "the launcher")
}

/// Every adjustment and constraint the launcher document itself carries:
/// the base `adjustments` list against the launcher's axes, plus the
/// adjustments and constraints of every INLINE fragment against the
/// launcher's axes and the fragment's own (file fragments are checked when
/// they are read, against the same reach).
pub(crate) fn validate_launcher_adjustments(
    adjustments: &[Adjustment],
    axes: &[ComponentAxis],
) -> Result<(), String> {
    let launcher_axes: Vec<&ComponentAxis> = axes.iter().collect();
    for adjustment in adjustments {
        validate_adjustment(adjustment, "the launcher's `adjustments`")?;
        if let Some(when) = &adjustment.when {
            validate_guard(when, &launcher_axes, "the launcher's `adjustments`")?;
        }
    }
    for axis in axes {
        for (option, spec) in &axis.options {
            let origin = format!("inline option `{}.{}`", axis.name, option);
            let own_axes: Vec<&ComponentAxis> = spec
                .0
                .iter()
                .filter_map(|part| match part {
                    FragmentPart::Inline(fragment) => Some(fragment.components.iter()),
                    FragmentPart::File(_) => None,
                })
                .flatten()
                .collect();
            let in_reach: Vec<&ComponentAxis> =
                launcher_axes.iter().copied().chain(own_axes).collect();
            for part in &spec.0 {
                let FragmentPart::Inline(fragment) = part else {
                    continue;
                };
                validate_fragment_references(fragment, &in_reach, &origin)?;
            }
        }
    }
    Ok(())
}

/// The reference checks on one fragment body once the axes in its reach are
/// known: its guards and constraints name those axes and their options.
pub(crate) fn validate_fragment_references(
    fragment: &Fragment,
    in_reach: &[&ComponentAxis],
    origin: &str,
) -> Result<(), String> {
    for adjustment in &fragment.adjustments {
        if let Some(when) = &adjustment.when {
            validate_guard(when, in_reach, origin)?;
        }
    }
    validate_constraint_references(&fragment.constraints, in_reach, origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fragment_spec(content: &str) -> Result<FragmentSpec, String> {
        serde_json5::from_str(content).map_err(|e| e.to_string())
    }

    #[test]
    fn a_path_option_parses_as_one_file_part() {
        let spec = parse_fragment_spec(r#""fragments/web_commander.json5""#).expect("path parses");
        assert!(matches!(
            spec.0.as_slice(),
            [FragmentPart::File(p)] if p == "fragments/web_commander.json5"
        ));
    }

    #[test]
    fn an_inline_option_parses_as_one_fragment_body() {
        let spec = parse_fragment_spec(r#"{ deployments: [], core_nodes: ["cloud"] }"#)
            .expect("inline fragment parses");
        let [FragmentPart::Inline(fragment)] = spec.0.as_slice() else {
            panic!("expected one inline part, got {spec:?}");
        };
        assert!(fragment.deployments.is_empty());
        assert_eq!(fragment.core_nodes, ["cloud"]);
    }

    #[test]
    fn a_list_option_keeps_its_order() {
        let spec = parse_fragment_spec(
            r#"["fragments/sim_relays.json5", "fragments/mujoco_engine.json5"]"#,
        )
        .expect("list parses");
        assert_eq!(spec.0.len(), 2);
        assert!(matches!(&spec.0[0], FragmentPart::File(p) if p == "fragments/sim_relays.json5"));
        assert!(
            matches!(&spec.0[1], FragmentPart::File(p) if p == "fragments/mujoco_engine.json5")
        );
    }

    #[test]
    fn a_mixed_list_parses_both_kinds() {
        let spec = parse_fragment_spec(
            r#"["fragments/relays.json5", { deployments: [], core_nodes: ["edge"] }]"#,
        )
        .expect("mixed list parses");
        assert!(matches!(&spec.0[0], FragmentPart::File(_)));
        assert!(matches!(&spec.0[1], FragmentPart::Inline(_)));
    }

    #[test]
    fn an_empty_fragment_list_is_refused() {
        let error = parse_fragment_spec("[]").expect_err("empty list must be refused");
        assert!(error.contains("cannot be empty"), "got: {error}");
    }

    #[test]
    fn an_empty_path_is_refused() {
        let error = parse_fragment_spec(r#""   ""#).expect_err("empty path must be refused");
        assert!(error.contains("cannot be empty"), "got: {error}");
    }

    #[test]
    fn an_inline_fragment_cannot_declare_a_schema() {
        let error = parse_fragment_spec(r#"{ peppy_schema: "launcher/v1", deployments: [] }"#)
            .expect_err("inline fragments take no peppy_schema");
        assert!(error.contains("peppy_schema"), "got: {error}");
    }

    #[test]
    fn a_fragment_spec_round_trips_through_its_written_shape() {
        for written in [
            r#""fragments/a.json5""#.to_owned(),
            r#"{ deployments: [] }"#.to_owned(),
            r#"["fragments/a.json5", { deployments: [] }]"#.to_owned(),
        ] {
            let spec = serde_json5::from_str::<FragmentSpec>(&written).expect("parses");
            let reserialized = serde_json5::to_string(&spec).expect("serializes");
            let reparsed = serde_json5::from_str::<FragmentSpec>(&reserialized).expect("reparses");
            assert_eq!(spec.0.len(), reparsed.0.len(), "round trip of {written}");
        }
    }

    #[test]
    fn a_fragment_file_document_parses_with_its_own_schema() {
        let content = r#"{
            peppy_schema: "launcher_fragment/v1",
            deployments: [],
        }"#;
        let fragment = LauncherFragmentParser::from_content(content).expect("fragment parses");
        assert_eq!(fragment.peppy_schema, PeppySchema::LauncherFragmentV1);
        assert!(fragment.body.deployments.is_empty());
    }

    #[test]
    fn a_fragment_file_refuses_an_unknown_field() {
        let error = LauncherFragmentParser::from_content(
            r#"{ peppy_schema: "launcher_fragment/v1", deploymnts: [] }"#,
        )
        .expect_err("a misspelled field must not silently vanish");
        assert!(error.to_string().contains("deploymnts"), "got: {error}");
    }

    #[test]
    fn a_fragment_file_refuses_the_launcher_schema() {
        let error = LauncherFragmentParser::from_content(
            r#"{ peppy_schema: "launcher/v1", deployments: [] }"#,
        )
        .expect_err("a launcher is not a fragment");
        assert!(
            error.to_string().contains("launcher_fragment/v1"),
            "got: {error}"
        );
    }

    // -- a fragment's own axes and deployed options ---------------------------

    fn parse_fragment(body: &str) -> Result<LauncherFragment, String> {
        LauncherFragmentParser::from_content(&format!(
            r#"{{ peppy_schema: "launcher_fragment/v1", {body} }}"#
        ))
        .map_err(|e| e.to_string())
    }

    #[test]
    fn a_fragment_declares_axes_and_deploys_one_of_their_options() {
        let fragment = parse_fragment(
            r#"components: [
                 { name: "commander", provides: ["commander_inst"],
                   options: { web: "web.json5", xr: "xr.json5" } },
                 { name: "recorder", cardinality: "zero_or_one", options: { on: "rec.json5" } },
               ],
               deployments: [
                 { source: { name: "backbone", tag: "v1" }, instances: [{ instance_id: "backbone_inst" }] },
                 { commander: "web" },
               ]"#,
        )
        .expect("a fragment with axes parses");
        assert_eq!(fragment.body.components.len(), 2);
        assert_eq!(fragment.body.deployments.len(), 1);
        assert_eq!(
            fragment.body.option_deployments,
            [OptionDeployment {
                axis: "commander".into(),
                option: "web".into(),
                with: BTreeMap::new(),
                arguments: BTreeMap::new(),
                adjustments: Vec::new(),
                instances: Vec::new()
            }]
        );
    }

    #[test]
    fn a_fragment_axis_cannot_be_repeatable() {
        let error = parse_fragment(
            r#"components: [{ name: "cameras", cardinality: "zero_or_more", options: { a: "a.json5" } }]"#,
        )
        .expect_err("copies belong to the launcher");
        assert!(error.contains("zero_or_more"), "got: {error}");
        assert!(error.contains("launcher"), "got: {error}");
    }

    #[test]
    fn a_fragment_serializes_its_deployments_as_one_list() {
        let fragment = parse_fragment(
            r#"components: [{ name: "commander", options: { web: "web.json5" } }],
               deployments: [
                 { source: { name: "backbone", tag: "v1" }, instances: [{ instance_id: "backbone_inst" }] },
                 { commander: "web" },
               ]"#,
        )
        .unwrap();
        let written = serde_json5::to_string(&fragment.body).unwrap();
        let reparsed: Fragment = serde_json5::from_str(&written).unwrap();
        assert_eq!(reparsed.deployments.len(), 1);
        assert_eq!(
            reparsed.option_deployments,
            fragment.body.option_deployments
        );
    }

    // -- deployment entries ---------------------------------------------------

    fn parse_entries(list: &str) -> Result<DeploymentEntries, String> {
        serde_json5::from_str(list).map_err(|e| e.to_string())
    }

    #[test]
    fn a_node_entry_and_an_option_entry_read_by_their_keys() {
        let entries = parse_entries(
            r#"[
                { source: { name: "engine", tag: "v1" }, instances: [{ instance_id: "engine_inst" }] },
                { simulation: "waldo" },
                { robot: "openarm_v2_sim", instances: [
                    { instance_id: "alpha", with: { commander: "web" },
                      arguments: { commander_inst: { http_port: 8765 } } } ] },
            ]"#,
        )
        .expect("entries parse");
        assert_eq!(entries.nodes.len(), 1);
        assert_eq!(entries.options.len(), 2);
        let robot = &entries.options[1];
        assert_eq!(
            (robot.axis.as_str(), robot.option.as_str()),
            ("robot", "openarm_v2_sim")
        );
        let alpha = &robot.instances[0];
        assert_eq!(alpha.instance_id.as_str(), "alpha");
        assert_eq!(alpha.with["commander"], "web");
        assert_eq!(
            alpha.arguments["commander_inst"]["http_port"],
            AnyType::Int(8765)
        );
    }

    #[test]
    fn an_entry_naming_a_node_without_source_says_the_shape() {
        let error = parse_entries(r#"[{ name: "engine", tag: "v1", instances: [] }]"#)
            .expect_err("a node deployment has a source");
        assert!(error.contains("under `source`"), "got: {error}");
        assert!(error.contains("deployments[0]"), "got: {error}");
    }

    #[test]
    fn an_entry_naming_two_components_is_refused() {
        let error = parse_entries(r#"[{ simulation: "waldo", robot: "sim" }]"#)
            .expect_err("one component per entry");
        assert!(
            error.contains("`simulation`") && error.contains("`robot`"),
            "got: {error}"
        );
    }

    #[test]
    fn an_option_that_is_not_a_string_is_refused() {
        let error =
            parse_entries(r#"[{ simulation: ["waldo"] }]"#).expect_err("an option is a string");
        assert!(error.contains("as a string"), "got: {error}");
    }

    #[test]
    fn a_copy_cannot_declare_its_own_placement_or_links() {
        let error = parse_entries(
            r#"[{ robot: "sim", instances: [{ instance_id: "alpha", core_node: "jetson" }] }]"#,
        )
        .expect_err("a copy is placed by name");
        assert!(error.contains("--place alpha@CORE_NODE"), "got: {error}");
        let error = parse_entries(
            r#"[{ robot: "sim", instances: [{ instance_id: "alpha", links: { a: "b" } }] }]"#,
        )
        .expect_err("a copy's wiring is its fragment's");
        assert!(error.contains("`links`"), "got: {error}");
    }

    #[test]
    fn a_copy_override_naming_no_argument_is_refused() {
        let error = parse_entries(
            r#"[{ robot: "sim", instances: [{ instance_id: "alpha", arguments: { commander_inst: {} } }] }]"#,
        )
        .expect_err("an empty override says nothing");
        assert!(error.contains("overrides no argument"), "got: {error}");
    }

    // -- validate_axes ------------------------------------------------------

    fn launcher_axes(body: &str) -> Result<Vec<ComponentAxis>, String> {
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str(&format!("[{body}]")).map_err(|e| e.to_string())?;
        validate_axes(&axes, AxisScope::Launcher).map(|()| axes)
    }

    #[test]
    fn a_well_formed_axis_passes() {
        let axes = launcher_axes(
            r#"{ name: "robot", provides: ["left_arm_inst"],
                 options: { real: { deployments: [] } } }"#,
        )
        .expect("valid axis");
        assert_eq!(axes[0].name, "robot");
    }

    #[test]
    fn a_default_is_refused_and_says_where_it_went() {
        let error = launcher_axes(
            r#"{ name: "robot", default: "real", options: { real: { deployments: [] } } }"#,
        )
        .expect_err("default has left the grammar");
        assert!(error.contains("`default`"), "got: {error}");
        assert!(error.contains(r#"{ robot: "<option>" }"#), "got: {error}");
    }

    #[test]
    fn an_unwireable_axis_name_is_refused() {
        for name in ["has space", "has=equals", "has,comma"] {
            let error = launcher_axes(&format!(
                r#"{{ name: "{name}", options: {{ real: {{ deployments: [] }} }} }}"#
            ))
            .expect_err("an unwireable name must be refused");
            assert!(error.contains(name), "got: {error}");
        }
    }

    #[test]
    fn a_reserved_axis_name_is_refused() {
        for name in RESERVED_COMPONENT_NAMES {
            let error = launcher_axes(&format!(
                r#"{{ name: "{name}", options: {{ real: {{ deployments: [] }} }} }}"#
            ))
            .expect_err("a deployments key cannot be an axis");
            assert!(error.contains("cannot be a component name"), "got: {error}");
        }
    }

    #[test]
    fn an_axis_with_no_options_is_refused() {
        let error = launcher_axes(r#"{ name: "robot", options: {} }"#)
            .expect_err("an empty options map must be refused");
        assert!(error.contains("declares no `options`"), "got: {error}");
    }

    #[test]
    fn a_duplicated_axis_name_is_refused() {
        let error = launcher_axes(
            r#"{ name: "robot", options: { a: { deployments: [] } } },
                { name: "robot", options: { b: { deployments: [] } } }"#,
        )
        .expect_err("duplicate axis names must be refused");
        assert!(error.contains("more than once"), "got: {error}");
    }

    #[test]
    fn an_axis_may_share_a_name_with_its_own_option() {
        launcher_axes(
            r#"{ name: "cameras", cardinality: "zero_or_one", options: { cameras: { deployments: [] } } }"#,
        )
        .expect("a single-option `zero_or_one` axis is a feature toggle");
    }

    #[test]
    fn an_axis_sharing_another_axis_option_name_is_refused() {
        let error = launcher_axes(
            r#"{ name: "robot", options: { real: { deployments: [] } } },
                { name: "real", options: { x: { deployments: [] } } }"#,
        )
        .expect_err("a bare --with word must resolve to one thing");
        assert!(error.contains("`robot`"), "got: {error}");
        assert!(error.contains("`real`"), "got: {error}");
    }

    // -- validate_option_deployments -------------------------------------------

    fn deployed(axes: &str, entries: &str) -> Result<(), String> {
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str(&format!("[{axes}]")).map_err(|e| e.to_string())?;
        let entries: DeploymentEntries =
            serde_json5::from_str(&format!("[{entries}]")).map_err(|e| e.to_string())?;
        validate_option_deployments(&entries.options, &axes)
    }

    const SIM_AXIS: &str = r#"{ name: "simulation", options: { waldo: {}, mujoco: {} } }"#;
    const ROBOT_AXIS: &str =
        r#"{ name: "robot", cardinality: "zero_or_more", options: { sim: {}, real: {} } }"#;

    #[test]
    fn a_one_axis_deploys_one_option_without_copies() {
        deployed(SIM_AXIS, r#"{ simulation: "waldo" }"#).expect("deploys once");
        let error = deployed(
            SIM_AXIS,
            r#"{ simulation: "waldo", instances: [{ instance_id: "a" }] }"#,
        )
        .expect_err("a `one` axis has no copies");
        assert!(error.contains("takes no `instances`"), "got: {error}");
        let error = deployed(
            SIM_AXIS,
            r#"{ simulation: "waldo" }, { simulation: "mujoco" }"#,
        )
        .expect_err("one option per `one` axis");
        assert!(error.contains("twice"), "got: {error}");
    }

    #[test]
    fn a_zero_or_one_axis_cannot_be_deployed_by_the_file() {
        let error = deployed(
            r#"{ name: "recorder", cardinality: "zero_or_one", options: { on: {} } }"#,
            r#"{ recorder: "on" }"#,
        )
        .expect_err("nothing could switch it off");
        assert!(error.contains("--with on"), "got: {error}");
    }

    #[test]
    fn a_repeatable_axis_deploys_named_copies() {
        deployed(
            ROBOT_AXIS,
            r#"{ robot: "sim", instances: [{ instance_id: "alpha" }, { instance_id: "bravo" }] }"#,
        )
        .expect("copies deploy");
        let error = deployed(ROBOT_AXIS, r#"{ robot: "sim" }"#).expect_err("copies need names");
        assert!(error.contains("instance_id"), "got: {error}");
        let error = deployed(
            ROBOT_AXIS,
            r#"{ robot: "sim", instances: [{ instance_id: "alpha" }] },
               { robot: "real", instances: [{ instance_id: "alpha" }] }"#,
        )
        .expect_err("copy names are unique");
        assert!(error.contains("twice"), "got: {error}");
    }

    #[test]
    fn an_entry_naming_an_unknown_axis_or_option_lists_the_choices() {
        let error = deployed(SIM_AXIS, r#"{ scene: "x" }"#).expect_err("unknown axis");
        assert!(error.contains("`simulation`"), "got: {error}");
        let error = deployed(SIM_AXIS, r#"{ simulation: "isaac" }"#).expect_err("unknown option");
        assert!(error.contains("`waldo`"), "got: {error}");
    }

    // -- validate_constraints -----------------------------------------------

    /// Two axes shaped like the cases constraints exist for: a choice and an
    /// optional feature toggle.
    fn constraint_axes() -> Vec<ComponentAxis> {
        serde_json5::from_str(
            r#"[
                { name: "robot",
                  options: { real: { deployments: [] }, mujoco: { deployments: [] } } },
                { name: "recorder", cardinality: "zero_or_one",
                  options: { on: { deployments: [] } } },
            ]"#,
        )
        .expect("axes parse")
    }

    fn one_constraint(body: &str) -> Result<(), String> {
        let constraints: Vec<SelectionConstraint> =
            serde_json5::from_str(&format!("[{body}]")).map_err(|e| e.to_string())?;
        validate_launcher_constraints(&constraints, &constraint_axes())
    }

    #[test]
    fn a_well_formed_constraint_passes() {
        one_constraint(
            r#"{ when: { recorder: "on" },
                 requires: [{ robot: "real" }],
                 reason: "the recorder films the physical rig" }"#,
        )
        .expect("valid constraint");
    }

    #[test]
    fn an_unconditional_constraint_passes() {
        one_constraint(r#"{ requires: [{ robot: "real" }, { recorder: "on" }], reason: "why" }"#)
            .expect("a constraint without `when` speaks about every selection");
    }

    #[test]
    fn an_empty_constraint_guard_is_refused() {
        let error = one_constraint(r#"{ when: {}, requires: [{ robot: "real" }], reason: "r" }"#)
            .expect_err("an empty guard means nothing");
        assert!(error.contains("empty `when`"), "got: {error}");
    }

    #[test]
    fn a_constraint_enforcing_nothing_is_refused() {
        let error = one_constraint(r#"{ when: { recorder: "on" }, requires: [], reason: "r" }"#)
            .expect_err("nothing to enforce refuses nothing");
        assert!(
            error.contains("neither `requires` nor `forbids`"),
            "got: {error}"
        );
    }

    #[test]
    fn a_forbids_only_constraint_passes() {
        one_constraint(
            r#"{ when: { robot: "mujoco" }, forbids: [{ recorder: "on" }],
                 reason: "the recorder films only the physical rig" }"#,
        )
        .expect("an exclusion needs no requires");
        one_constraint(r#"{ forbids: [{ robot: "mujoco", recorder: "on" }], reason: "why" }"#)
            .expect("an unconditional exclusion of one combination");
    }

    #[test]
    fn an_empty_forbids_entry_is_refused() {
        let error = one_constraint(r#"{ when: { robot: "mujoco" }, forbids: [{}], reason: "r" }"#)
            .expect_err("an empty entry says nothing");
        assert!(error.contains("empty `forbids` entry"), "got: {error}");
    }

    #[test]
    fn a_forbids_entry_on_a_guarded_axis_is_refused() {
        let error = one_constraint(
            r#"{ when: { robot: "mujoco" }, forbids: [{ robot: "real" }], reason: "r" }"#,
        )
        .expect_err("under the guard the axis is already fixed");
        assert!(error.contains("already fixes"), "got: {error}");
    }

    #[test]
    fn a_duplicated_forbids_entry_is_refused() {
        let error =
            one_constraint(r#"{ forbids: [{ recorder: "on" }, { recorder: "on" }], reason: "r" }"#)
                .expect_err("a duplicate entry is a mistake");
        assert!(error.contains("more than once"), "got: {error}");
    }

    #[test]
    fn a_map_in_both_requires_and_forbids_is_refused() {
        let error = one_constraint(
            r#"{ requires: [{ recorder: "on" }], forbids: [{ recorder: "on" }], reason: "r" }"#,
        )
        .expect_err("one map cannot be both the satisfaction and the refusal");
        assert!(error.contains("both what satisfies"), "got: {error}");
    }

    /// The conflict is the IDENTICAL map in both lists: different maps may
    /// share one constraint, each list keeping its own meaning.
    #[test]
    fn requires_and_forbids_with_different_maps_coexist() {
        one_constraint(
            r#"{ requires: [{ robot: "real" }], forbids: [{ recorder: "on" }],
                 reason: "the rig runs unrecorded" }"#,
        )
        .expect("only the identical map in both lists is a conflict");
    }

    #[test]
    fn a_forbids_entry_naming_an_unknown_option_is_refused() {
        let error = one_constraint(r#"{ forbids: [{ robot: "genesis" }], reason: "r" }"#)
            .expect_err("an unknown option is a dead reference");
        assert!(error.contains("`genesis`"), "got: {error}");
        assert!(error.contains("`forbids` entry"), "got: {error}");
    }

    #[test]
    fn an_empty_requires_alternative_is_refused() {
        let error = one_constraint(r#"{ when: { recorder: "on" }, requires: [{}], reason: "r" }"#)
            .expect_err("an empty alternative says nothing");
        assert!(
            error.contains("empty `requires` alternative"),
            "got: {error}"
        );
    }

    #[test]
    fn a_requires_alternative_on_a_guarded_axis_is_refused() {
        let error = one_constraint(
            r#"{ when: { robot: "mujoco" }, requires: [{ robot: "real" }], reason: "r" }"#,
        )
        .expect_err("under the guard the axis is already fixed");
        assert!(error.contains("already fixes"), "got: {error}");
    }

    #[test]
    fn a_duplicated_requires_alternative_is_refused() {
        let error = one_constraint(
            r#"{ when: { recorder: "on" },
                 requires: [{ robot: "real" }, { robot: "real" }], reason: "r" }"#,
        )
        .expect_err("a duplicate alternative is a mistake");
        assert!(error.contains("more than once"), "got: {error}");
    }

    #[test]
    fn a_blank_constraint_reason_is_refused() {
        let error = one_constraint(
            r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }], reason: "   " }"#,
        )
        .expect_err("the refusal quotes the reason");
        assert!(error.contains("empty `reason`"), "got: {error}");
    }

    #[test]
    fn a_constraint_without_a_reason_does_not_parse() {
        let error =
            one_constraint(r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }] }"#)
                .expect_err("reason is required");
        assert!(error.contains("reason"), "got: {error}");
    }

    #[test]
    fn a_constraint_naming_an_unknown_axis_or_option_is_refused() {
        let error = one_constraint(
            r#"{ when: { commander: "web" }, requires: [{ robot: "real" }], reason: "r" }"#,
        )
        .expect_err("an unknown axis is a dead reference");
        assert!(error.contains("`commander`"), "got: {error}");

        let error = one_constraint(
            r#"{ when: { recorder: "on" }, requires: [{ robot: "sim" }], reason: "r" }"#,
        )
        .expect_err("an unknown option is a dead reference");
        assert!(error.contains("`sim`"), "got: {error}");
        assert!(error.contains("`requires` alternative"), "got: {error}");
    }

    #[test]
    fn a_misspelled_constraint_field_is_refused() {
        let error = one_constraint(
            r#"{ when: { recorder: "on" }, requires: [{ robot: "real" }], reasn: "r" }"#,
        )
        .expect_err("a misspelled field must not silently vanish");
        assert!(error.contains("reasn"), "got: {error}");
    }

    // -- validate_adjustment ------------------------------------------------

    fn parse_adjustment(body: &str) -> Result<Adjustment, String> {
        let adjustment: Adjustment = serde_json5::from_str(body).map_err(|e| e.to_string())?;
        validate_adjustment(&adjustment, "a test fragment").map(|()| adjustment)
    }

    #[test]
    fn a_complete_adjustment_parses() {
        let adjustment = parse_adjustment(
            r#"{
                target: "backbone_inst",
                when: { commander: "xr_commander" },
                set_arguments: { upstream_mode: "pose" },
                set_links: { collision_ctrl: { vacant: "no producer here" } },
                add_links: { recorder: ["recorder_inst"] },
                unset_links: ["leader_left_arm_pose"],
            }"#,
        )
        .expect("a full adjustment parses");
        assert_eq!(adjustment.target.as_str(), "backbone_inst");
        assert_eq!(
            adjustment
                .when
                .as_ref()
                .and_then(|w| w.get("commander"))
                .map(|options| options.iter().collect::<Vec<_>>()),
            Some(vec!["xr_commander"])
        );
    }

    #[test]
    fn an_adjustment_with_no_operation_is_refused() {
        let error = parse_adjustment(r#"{ target: "backbone_inst" }"#)
            .expect_err("an adjustment must do something");
        assert!(error.contains("names no operation"), "got: {error}");
    }

    #[test]
    fn an_empty_guard_is_refused() {
        let error =
            parse_adjustment(r#"{ target: "backbone_inst", when: {}, set_arguments: { a: 1 } }"#)
                .expect_err("an empty guard means nothing");
        assert!(error.contains("empty `when`"), "got: {error}");
    }

    #[test]
    fn add_links_targets_are_shape_checked() {
        let error = parse_adjustment(r#"{ target: "c", add_links: { cameras: [] } }"#)
            .expect_err("an empty target list appends nothing");
        assert!(error.contains("no target"), "got: {error}");

        let error = parse_adjustment(r#"{ target: "c", add_links: { cameras: ["a", "a"] } }"#)
            .expect_err("a duplicate target is a mistake");
        assert!(error.contains("more than once"), "got: {error}");

        let error = parse_adjustment(r#"{ target: "c", add_links: { cameras: ["a//b"] } }"#)
            .expect_err("a malformed target is refused");
        assert!(error.contains("malformed"), "got: {error}");
    }

    #[test]
    fn a_guard_naming_an_unknown_axis_is_refused() {
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str(r#"[{ name: "robot", options: { real: { deployments: [] } } }]"#)
                .expect("axes parse");
        let in_reach: Vec<&ComponentAxis> = axes.iter().collect();
        let error = validate_guard(
            &serde_json5::from_str("{ commander: 'web' }").unwrap(),
            &in_reach,
            "a test fragment",
        )
        .expect_err("an unknown axis is a dead reference");
        assert!(error.contains("`commander`"), "got: {error}");
        assert!(error.contains("`robot`"), "got: {error}");
    }

    #[test]
    fn a_guard_naming_an_unknown_option_is_refused() {
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str(r#"[{ name: "robot", options: { real: { deployments: [] } } }]"#)
                .expect("axes parse");
        let in_reach: Vec<&ComponentAxis> = axes.iter().collect();
        let error = validate_guard(
            &serde_json5::from_str("{ robot: 'sim' }").unwrap(),
            &in_reach,
            "a test fragment",
        )
        .expect_err("an unknown option is a dead reference");
        assert!(error.contains("`sim`"), "got: {error}");
    }

    #[test]
    fn condition_options_are_nonempty_distinct_names_and_round_trip() {
        for input in ["[]", "['real', 'real']", "['bad/name']", "[1]", "null"] {
            assert!(
                serde_json5::from_str::<ConditionOptions>(input).is_err(),
                "{input}"
            );
        }
        for input in ["'real'", "['real']", "['sim', 'real']"] {
            let options: ConditionOptions = serde_json5::from_str(input).unwrap();
            assert!(options.contains("real"));
            let encoded = serde_json5::to_string(&options).unwrap();
            assert_eq!(
                serde_json5::from_str::<ConditionOptions>(&encoded).unwrap(),
                options
            );
        }
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str("[{ name: 'robot', options: { real: { deployments: [] } } }]")
                .unwrap();
        let in_reach: Vec<&ComponentAxis> = axes.iter().collect();
        let guard = serde_json5::from_str("{ robot: ['real', 'sim'] }").unwrap();
        assert!(
            validate_guard(&guard, &in_reach, "test")
                .unwrap_err()
                .contains("`sim`")
        );
    }
}
