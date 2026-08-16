//! The composition grammar: the `components` axes and `adjustments` a
//! `launcher/v1` document may declare, and the `launcher_fragment/v1`
//! documents its options reference.
//!
//! A launcher without `components` is a flat stack and never reaches this
//! module's types. A launcher with them describes a FAMILY of stacks whose
//! members differ in which implementation fills each axis, selected at launch
//! time with `--with`. Flattening ( [`super::flatten`] ) turns a launcher plus
//! a selection into the ordinary flat document the rest of the pipeline
//! consumes; this module is only the grammar and the checks that need no I/O
//! and no selection.

use super::types::{Deployment, LinkValue};
use config::{AnyType, runtime::Name, schema::PeppySchema};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, SeqAccess, Visitor},
    ser::SerializeSeq,
};
use std::collections::{BTreeMap, HashSet};

/// One axis declaration in a launcher's `components` list.
///
/// The list is ORDERED, and the order is load-bearing: it fixes the order
/// fragments are collected in and therefore the order adjustments apply and
/// deployments appear, the same way `deployments` order fixes start order.
/// An axis names the alternative `options` that fill it; exactly one option
/// per axis is ever selected (§ "an axis takes one option, never two").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentAxis {
    /// The axis's name, unique within the launcher and spelled with the same
    /// identifier grammar as every other name in a peppy document. Used by
    /// `--with axis=option` and by `when` guards.
    pub name: String,
    /// The alternatives that fill this axis, at least one. A value is an
    /// inline fragment object, a path relative to the launcher's own
    /// directory, or a list mixing both (merged in list order).
    pub options: BTreeMap<String, FragmentSpec>,
    /// Chosen when `--with` names nothing on this axis. Forbidden together
    /// with `optional`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// When true the axis may be left unfilled: an unselected optional axis
    /// contributes nothing to the stack.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// The axis's interface: every option must define all of these instance
    /// ids in its deployments, so a link or adjustment written against one is
    /// valid regardless of which option the operator picks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<Name>,
}

/// One option's fragment set, in the order the option declared it: an inline
/// fragment, a path, or a list mixing both all parse into the same shape.
///
/// The two origins parse identically but live differently: an inline fragment
/// is part of the launcher file, while a path is resolved and read when the
/// composition is loaded ( [`super::flatten`] ), never here.
#[derive(Debug, Clone, Default)]
pub struct FragmentSpec(pub Vec<FragmentPart>);

/// One piece of an option's body: a fragment written inline, or a path to a
/// `launcher_fragment/v1` file.
#[derive(Debug, Clone)]
pub enum FragmentPart {
    Inline(Fragment),
    File(String),
}

/// The body of one fragment: what a `--with` selection pulls in.
///
/// `deployments` uses the same grammar as the launcher's own, `core_nodes`
/// are unioned with the base's, and `adjustments` reach instances the
/// fragment does not own. A fragment cannot declare `components` (no
/// nesting) and cannot declare `peppy_schema` when inline: the file form
/// wraps this body in [`LauncherFragment`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<Deployment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adjustments: Vec<Adjustment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_nodes: Vec<String>,
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
            deployments: Vec<Deployment>,
            #[serde(default)]
            adjustments: Vec<Adjustment>,
            #[serde(default)]
            core_nodes: Vec<String>,
        }

        let raw = RawLauncherFragment::deserialize(deserializer)?;
        Ok(LauncherFragment {
            peppy_schema: raw.peppy_schema,
            body: Fragment {
                deployments: raw.deployments,
                adjustments: raw.adjustments,
                core_nodes: raw.core_nodes,
            },
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

/// One rule about which selections this launcher refuses to be: `when` a
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
/// Constraints live in the launcher, not in fragments: they speak in axis
/// and option names, which are the launcher's vocabulary (fragments are
/// deliberately reusable across launchers whose axes differ).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionConstraint {
    /// Guard: the selections this constraint speaks about, in the same
    /// `axis: option` grammar as an adjustment's `when` (several pairs are an
    /// AND). Absent means every selection; an unfilled optional axis matches
    /// no pair, so a constraint guarded on its option stays quiet when the
    /// axis is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<BTreeMap<String, String>>,
    /// The alternatives, at least one of which the selection must satisfy
    /// wholly: each is an `axis: option` map (an AND), and listing several is
    /// an OR. An unfilled optional axis satisfies no pair, which is exactly
    /// how "this option needs a consumer" is written: require the axes that
    /// can consume it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<BTreeMap<String, String>>,
    /// The combinations the guarded selections must not contain: each entry
    /// is an `axis: option` map (an AND), and matching any entry refuses the
    /// selection. The direct form of "these options never launch together",
    /// which `requires` cannot say about an OPTIONAL axis's option (nothing
    /// requirable means "that axis stays off"). Where the axis is required,
    /// prefer requiring the wanted option instead: `requires` fails closed
    /// for options added later, while a `forbids` list names today's options
    /// and stays silent about tomorrow's.
    ///
    /// A constraint states at least one of `requires` and `forbids`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbids: Vec<BTreeMap<String, String>>,
    /// Why the refused selections must not launch, quoted verbatim in the
    /// refusal. Required, like a vacancy's reason: the engine can render
    /// what was required, but only the author knows what the dead
    /// combination would have done.
    pub reason: String,
}

/// One change to an instance defined elsewhere, in the base or in another
/// selected fragment. Every change names its operation; nothing is inferred
/// from a value's JSON type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adjustment {
    /// The instance to change. Whether an instance exists is declared by the
    /// base and the axes' options, so an adjustment whose target the resolved
    /// selection does not define is skipped, not refused; a target no option
    /// of the launcher defines anywhere is a dead reference and a parse error.
    pub target: Name,
    /// Guard: every named axis must match the named option for this
    /// adjustment to run. Absent means unconditional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<BTreeMap<String, String>>,
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
/// own `launcher_fragment/v1` file next to the launcher.
fn fragment_part_from_str<E: de::Error>(v: &str) -> Result<FragmentPart, E> {
    if v.trim().is_empty() {
        return Err(de::Error::custom(
            "a fragment path cannot be empty: name a `launcher_fragment/v1` file relative to \
             the launcher's directory",
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
            /// contribution enters flattening.
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut parts = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(part) = seq.next_element::<FragmentPart>()? {
                    parts.push(part);
                }
                if parts.is_empty() {
                    return Err(de::Error::custom(
                        "a fragment list cannot be empty: an option with no body has nothing to \
                         contribute, and an off-by-default feature is written as an optional axis \
                         instead",
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
/// contributes to the launcher whose option references it.
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

/// The launcher-local checks on a `components` list: the ones needing no
/// fragment file and no selection. Runs while the launcher document parses,
/// so an author hears them from any reader of the file.
///
/// What it deliberately does NOT check: that an option defines its axis's
/// `provides` ids (needs the fragments loaded, [`super::flatten`]) and that
/// `when` guards name real axes and options (checked for base and inline
/// adjustments here, for file fragments when they are read).
pub(crate) fn validate_axes(axes: &[ComponentAxis]) -> Result<(), String> {
    let mut axis_names = HashSet::with_capacity(axes.len());
    for axis in axes {
        check_axis_or_option_name("an axis", &axis.name)?;
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
        if axis.optional && axis.default.is_some() {
            return Err(format!(
                "axis `{}` declares both `optional: true` and a `default`: an optional axis is \
                 left unfilled by omitting it, so it has nothing to fall back to",
                axis.name
            ));
        }
        if let Some(default) = &axis.default
            && !axis.options.contains_key(default)
        {
            return Err(format!(
                "axis `{}` names `{}` as its `default`, which is not one of its options: {}",
                axis.name,
                default,
                crate::error::format_quoted_list(axis.options.keys())
            ));
        }
    }

    // A bare `--with` word resolves against option names, so an axis name
    // sharing a name with another axis's option would make that word name two
    // different things. An axis and its OWN option may share a name: a
    // single-option optional axis is a feature toggle, and both readings of
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
    Ok(())
}

impl ComponentAxis {
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

/// The shape checks on one adjustment: names that say something, operations
/// that exist, targets that are well-formed. Selection-independent, so they
/// run wherever the adjustment is read (launcher parse for the base and
/// inline fragments, fragment load for files).
pub(crate) fn validate_adjustment(adjustment: &Adjustment, origin: &str) -> Result<(), String> {
    if let Some(when) = &adjustment.when
        && when.is_empty()
    {
        return Err(format!(
            "adjustment on `{}` in {origin} declares an empty `when`: a guard names at least \
             one `axis: option` pair, or omits `when` entirely",
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

/// That a `when` guard names axes this launcher declares and options those
/// axes declare. A guard is what licenses an adjustment to depend on the
/// shape those options define, so naming something else is a dead reference.
pub(crate) fn validate_guard(
    when: &BTreeMap<String, String>,
    axes: &[ComponentAxis],
    origin: &str,
) -> Result<(), String> {
    validate_selection_map(when, axes, "a `when` guard", origin)
}

/// The shared half of guard and constraint validation: every `axis: option`
/// pair names an axis this launcher declares and an option that axis
/// declares. `what` names the map's role in the error (a `when` guard, a
/// `requires` alternative), which is all that differs between them.
fn validate_selection_map(
    map: &BTreeMap<String, String>,
    axes: &[ComponentAxis],
    what: &str,
    origin: &str,
) -> Result<(), String> {
    for (axis_name, option_name) in map {
        let Some(axis) = axes.iter().find(|axis| axis.name == *axis_name) else {
            return Err(format!(
                "{what} in {origin} names axis `{axis_name}`, which this launcher does \
                 not declare. Axes: {}",
                crate::error::format_quoted_list(axes.iter().map(ComponentAxis::as_str))
            ));
        };
        if !axis.options.contains_key(option_name) {
            return Err(format!(
                "{what} in {origin} names option `{option_name}` on axis \
                 `{axis_name}`, which the axis does not declare. Its options: {}",
                crate::error::format_quoted_list(axis.options.keys())
            ));
        }
    }
    Ok(())
}

/// The launcher-local checks on a `constraints` list: shapes that say
/// something and references that resolve. Selection-independent, so it runs
/// while the launcher document parses; whether a given selection violates a
/// constraint is [`super::flatten`]'s question.
pub(crate) fn validate_constraints(
    constraints: &[SelectionConstraint],
    axes: &[ComponentAxis],
) -> Result<(), String> {
    for (index, constraint) in constraints.iter().enumerate() {
        let origin = format!("the launcher's `constraints` entry {}", index + 1);
        if let Some(when) = &constraint.when {
            if when.is_empty() {
                return Err(format!(
                    "{origin} declares an empty `when`: a guard names at least one \
                     `axis: option` pair, or omits `when` entirely to speak about every \
                     selection"
                ));
            }
            validate_selection_map(when, axes, "a `when` guard", &origin)?;
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
            let mut seen: Vec<&BTreeMap<String, String>> = Vec::with_capacity(entries.len());
            for entry in entries {
                if entry.is_empty() {
                    return Err(format!(
                        "{origin} lists an empty {kind}: an entry names at least one \
                         `axis: option` pair"
                    ));
                }
                validate_selection_map(entry, axes, &format!("a {kind}"), &origin)?;
                if let Some(when) = &constraint.when {
                    for axis_name in entry.keys() {
                        if when.contains_key(axis_name) {
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
                    "{origin} lists the same `axis: option` map in `requires` and `forbids`: \
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

/// Every adjustment the launcher document itself carries: the base
/// `adjustments` list, plus the adjustments of every INLINE fragment (file
/// fragments are checked when they are read, against the same axes).
pub(crate) fn validate_launcher_adjustments(
    adjustments: &[Adjustment],
    axes: &[ComponentAxis],
) -> Result<(), String> {
    for adjustment in adjustments {
        validate_adjustment(adjustment, "the launcher's `adjustments`")?;
        if let Some(when) = &adjustment.when {
            validate_guard(when, axes, "the launcher's `adjustments`")?;
        }
    }
    for axis in axes {
        for (option, spec) in &axis.options {
            for part in &spec.0 {
                if let FragmentPart::Inline(fragment) = part {
                    let origin = format!("inline option `{}.{}`", axis.name, option);
                    for adjustment in &fragment.adjustments {
                        validate_adjustment(adjustment, &origin)?;
                        if let Some(when) = &adjustment.when {
                            validate_guard(when, axes, &origin)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
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

    // -- validate_axes ------------------------------------------------------

    fn one_axis(body: &str) -> Result<Vec<ComponentAxis>, String> {
        let axes: Vec<ComponentAxis> =
            serde_json5::from_str(&format!("[{body}]")).map_err(|e| e.to_string())?;
        validate_axes(&axes).map(|()| axes)
    }

    #[test]
    fn a_well_formed_axis_passes() {
        let axes = one_axis(
            r#"{ name: "robot", provides: ["left_arm_inst"], default: "real",
                 options: { real: { deployments: [] } } }"#,
        )
        .expect("valid axis");
        assert_eq!(axes[0].name, "robot");
        assert_eq!(axes[0].default.as_deref(), Some("real"));
    }

    #[test]
    fn an_unwireable_axis_name_is_refused() {
        for name in ["has space", "has=equals", "has,comma"] {
            let error = one_axis(&format!(
                r#"{{ name: "{name}", options: {{ real: {{ deployments: [] }} }} }}"#
            ))
            .expect_err("an unwireable name must be refused");
            assert!(error.contains(name), "got: {error}");
        }
    }

    #[test]
    fn an_axis_with_no_options_is_refused() {
        let error = one_axis(r#"{ name: "robot", options: {} }"#)
            .expect_err("an empty options map must be refused");
        assert!(error.contains("declares no `options`"), "got: {error}");
    }

    #[test]
    fn optional_with_a_default_is_refused() {
        let error = one_axis(
            r#"{ name: "robot", optional: true, default: "real",
                 options: { real: { deployments: [] } } }"#,
        )
        .expect_err("optional and default are exclusive");
        assert!(error.contains("both"), "got: {error}");
    }

    #[test]
    fn a_default_naming_a_missing_option_is_refused() {
        let error = one_axis(
            r#"{ name: "robot", default: "sim", options: { real: { deployments: [] } } }"#,
        )
        .expect_err("a default must name a declared option");
        assert!(error.contains("`sim`"), "got: {error}");
        assert!(error.contains("`real`"), "got: {error}");
    }

    #[test]
    fn a_duplicated_axis_name_is_refused() {
        let error = one_axis(
            r#"{ name: "robot", options: { a: { deployments: [] } } },
                { name: "robot", options: { b: { deployments: [] } } }"#,
        )
        .expect_err("duplicate axis names must be refused");
        assert!(error.contains("more than once"), "got: {error}");
    }

    #[test]
    fn an_axis_may_share_a_name_with_its_own_option() {
        one_axis(
            r#"{ name: "cameras", optional: true, options: { cameras: { deployments: [] } } }"#,
        )
        .expect("a single-option optional axis is a feature toggle");
    }

    #[test]
    fn an_axis_sharing_another_axis_option_name_is_refused() {
        let error = one_axis(
            r#"{ name: "robot", options: { real: { deployments: [] } } },
                { name: "real", options: { x: { deployments: [] } } }"#,
        )
        .expect_err("a bare --with word must resolve to one thing");
        assert!(error.contains("`robot`"), "got: {error}");
        assert!(error.contains("`real`"), "got: {error}");
    }

    // -- validate_constraints -----------------------------------------------

    /// Two axes shaped like the cases constraints exist for: a defaulted
    /// choice and an optional feature toggle.
    fn constraint_axes() -> Vec<ComponentAxis> {
        serde_json5::from_str(
            r#"[
                { name: "robot", default: "real",
                  options: { real: { deployments: [] }, mujoco: { deployments: [] } } },
                { name: "recorder", optional: true,
                  options: { on: { deployments: [] } } },
            ]"#,
        )
        .expect("axes parse")
    }

    fn one_constraint(body: &str) -> Result<(), String> {
        let constraints: Vec<SelectionConstraint> =
            serde_json5::from_str(&format!("[{body}]")).map_err(|e| e.to_string())?;
        validate_constraints(&constraints, &constraint_axes())
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
        one_constraint(
            r#"{ requires: [{ robot: "real" }, { recorder: "on" }], reason: "why" }"#,
        )
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
        one_constraint(
            r#"{ forbids: [{ robot: "mujoco", recorder: "on" }], reason: "why" }"#,
        )
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
        let error = one_constraint(
            r#"{ forbids: [{ recorder: "on" }, { recorder: "on" }], reason: "r" }"#,
        )
        .expect_err("a duplicate entry is a mistake");
        assert!(error.contains("more than once"), "got: {error}");
    }

    #[test]
    fn a_map_in_both_requires_and_forbids_is_refused() {
        let error = one_constraint(
            r#"{ requires: [{ recorder: "on" }], forbids: [{ recorder: "on" }], reason: "r" }"#,
        )
        .expect_err("one map cannot be both the satisfaction and the refusal");
        assert!(
            error.contains("both what satisfies"),
            "got: {error}"
        );
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
        let error = one_constraint(
            r#"{ forbids: [{ robot: "genesis" }], reason: "r" }"#,
        )
        .expect_err("an unknown option is a dead reference");
        assert!(error.contains("`genesis`"), "got: {error}");
        assert!(error.contains("`forbids` entry"), "got: {error}");
    }

    #[test]
    fn an_empty_requires_alternative_is_refused() {
        let error =
            one_constraint(r#"{ when: { recorder: "on" }, requires: [{}], reason: "r" }"#)
                .expect_err("an empty alternative says nothing");
        assert!(error.contains("empty `requires` alternative"), "got: {error}");
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
                .map(String::as_str),
            Some("xr_commander")
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
        let error = validate_guard(
            &BTreeMap::from([("commander".to_owned(), "web".to_owned())]),
            &axes,
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
        let error = validate_guard(
            &BTreeMap::from([("robot".to_owned(), "sim".to_owned())]),
            &axes,
            "a test fragment",
        )
        .expect_err("an unknown option is a dead reference");
        assert!(error.contains("`sim`"), "got: {error}");
    }
}
