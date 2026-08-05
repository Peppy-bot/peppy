//! Errors for the daemon-side document models.
//!
//! `deserialize_json5_with_path`, the private `StructuredError` bridge, and
//! `parsing::read_non_empty_file` are deliberate small copies of their
//! `config` (peppy-config-model) counterparts: the shared crate keeps those
//! private to its own parsers, and duplicating a few dozen bounded lines
//! beats widening the shared public surface to internals. `ParsingError`
//! here carries the daemon-domain variants (deployment sources, launcher
//! instances, bindings); general document errors raised by the shared node
//! parser stay in `config::ParsingError`.

use config::node::Cardinality;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

/// Formats `items` as a `\n  - `-prefixed bulleted list (no leading or
/// trailing newline outside the bullets themselves). Used to render
/// validation/binding error collections inside parent diagnostic strings.
pub fn format_bulleted<T, I>(items: I) -> String
where
    T: core::fmt::Display,
    I: IntoIterator<Item = T>,
{
    items.into_iter().map(|e| format!("\n  - {e}")).collect()
}

/// Formats `items` as a comma-separated list of backtick-quoted names, for
/// naming a set of offenders inline in a diagnostic ("core nodes `a`, `b`").
/// Returns an empty string for an empty iterator; callers that need a word
/// there ("nothing") should check emptiness themselves.
pub fn format_quoted_list<T, I>(items: I) -> String
where
    T: core::fmt::Display,
    I: IntoIterator<Item = T>,
{
    items
        .into_iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Deserializes JSON5 content with field-path tracking.
///
/// On error, prepends the JSON path (e.g. `execution.run_cmd`) to standard
/// serde error messages. `StructuredError`s (custom validation) are propagated
/// unchanged since they already contain descriptive messages.
pub fn deserialize_json5_with_path<'de, T>(content: &'de str) -> Result<T>
where
    T: serde::de::Deserialize<'de>,
{
    // Phase 1: parse JSON5 syntax. If this fails, there's no field path.
    let mut deserializer = serde_json5::Deserializer::from_str(content)
        .map_err(|e| Error::Parsing(ParsingError::from(e)))?;

    // Phase 2: deserialize with path tracking.
    serde_path_to_error::deserialize(&mut deserializer).map_err(|path_err| {
        let path = path_err.path().to_string();
        let inner: serde_json5::Error = path_err.into_inner();

        match inner {
            serde_json5::Error::Message { ref msg, .. } => {
                // Check if it's a StructuredError (custom validation).
                // These already have rich messages; don't prepend path.
                if let Ok(structured) = serde_json5::from_str::<StructuredError>(msg) {
                    return Error::Parsing(ParsingError::from(structured));
                }

                // Standard serde error: prepend path if non-empty.
                let message = if path.is_empty() || path == "." {
                    msg.clone()
                } else {
                    format!("{path}: {msg}")
                };
                Error::Parsing(ParsingError::CannotParseConfig(message))
            }
        }
    })
}

/// Whether a declared slot is a node dep (matched by `(name, tag)` identity)
/// or a contract dep (matched against the producer's `manifest.implements`). Used
/// in error payloads so messages can name the expected category in singular
/// human form instead of leaking the `depends_on.nodes` / `depends_on.contracts`
/// field path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Node,
    Contract,
}

impl core::fmt::Display for SlotKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            SlotKind::Node => "node",
            SlotKind::Contract => "contract",
        })
    }
}

/// Payload for [`ParsingError::BindingTargetMismatch`]. Kept as a separate
/// struct (and boxed in the variant) so the seven `String` fields do not
/// inflate `ParsingError` past the `clippy::result_large_err` threshold.
#[derive(Debug, Clone, Error)]
#[error(
    "binding `{binding}` on instance `{owner_instance_id}`: target \
     `{target_instance_id}` deploys node `{actual_name}:{actual_tag}`, \
     but slot expects node `{expected_name}:{expected_tag}`"
)]
pub struct BindingTargetMismatch {
    pub owner_instance_id: String,
    pub binding: String,
    pub target_instance_id: String,
    pub expected_name: String,
    pub expected_tag: String,
    pub actual_name: String,
    pub actual_tag: String,
}

/// Payload for [`ParsingError::BindingContractNotImplemented`]. Raised when a
/// `--link` targets a contract slot but the producer's `manifest.implements`
/// list does not include the requested `(contract_name, contract_tag)`.
///
/// Boxed in the variant for the same `clippy::result_large_err` reason as the
/// other binding error payloads.
#[derive(Debug, Clone, Error)]
#[error(
    "binding `{binding}` on instance `{owner_instance_id}`: target \
     `{target_instance_id}` deploys `{producer_name}:{producer_tag}`, but \
     the slot requires contract `{contract_name}:{contract_tag}` (add it \
     to the producer's `manifest.implements`)"
)]
pub struct BindingContractNotImplemented {
    pub owner_instance_id: String,
    pub binding: String,
    pub target_instance_id: String,
    pub contract_name: String,
    pub contract_tag: String,
    pub producer_name: String,
    pub producer_tag: String,
}

/// Payload for [`ParsingError::DuplicateInstanceIdAcrossStack`]. Boxed in
/// the variant for the same `result_large_err` reason as the other binding
/// variants.
///
/// Two instances anywhere in the running stack (any `(node_name,
/// node_tag)`) share an `instance_id`. The binding model addresses
/// producers by `instance_id` only, so a stack-wide duplicate would make
/// `--link KEY@id` ambiguous.
#[derive(Debug, Clone, Error)]
#[error(
    "duplicate instance_id `{instance_id}`: used by both `{name_a}:{tag_a}` \
     and `{name_b}:{tag_b}` (instance_ids must be unique across the stack)"
)]
pub struct DuplicateInstanceIdAcrossStack {
    pub instance_id: String,
    pub name_a: String,
    pub tag_a: String,
    pub name_b: String,
    pub tag_b: String,
}

/// Payload for [`ParsingError::LinkUnknownSlot`]. A `links:` key (or
/// `--link KEY`) that names no declared slot of the instance's node across
/// any family: `depends_on.{nodes,contracts}` producer slots or
/// `depends_on.pairings` participant/observer slots. Every link key must be a
/// declared slot link_id; there are no free-form keys.
#[derive(Debug, Clone, Error)]
#[error(
    "link `{link}` on instance `{owner_instance_id}` names no \
     declared slot; declared link_ids: [{declared_link_ids}]"
)]
pub struct LinkUnknownSlot {
    pub owner_instance_id: String,
    pub link: String,
    pub declared_link_ids: String,
}

/// Payload for [`ParsingError::BindingSlotUnfulfilled`]. A declared
/// `depends_on` slot of cardinality `one` or `one_or_more` with no binding
/// entry. Only a `zero_or_more` slot may be left unbound (its empty set is
/// a valid value); there is no wildcard fallback for the others.
#[derive(Debug, Clone, Error)]
#[error(
    "instance `{owner_instance_id}` leaves slot `{link_id}` ({slot_kind} \
     `{slot_name}:{slot_tag}`, cardinality `{cardinality}`) unfulfilled: every \
     declared depends_on slot except `zero_or_more` must be bound; add a \
     `links:` entry for `{link_id}` (or `--link {link_id}@<producer_instance_id>`, \
     repeatable on a multi slot) or remove the dependency from the node manifest"
)]
pub struct BindingSlotUnfulfilled {
    pub owner_instance_id: String,
    pub link_id: String,
    pub slot_kind: SlotKind,
    pub slot_name: String,
    pub slot_tag: String,
    pub cardinality: Cardinality,
}

/// Payload for [`ParsingError::PairingTargetNotComplementary`]. The target
/// instance declares no unclaimed pairing slot with the same `(name, tag)`
/// and the opposite role.
#[derive(Debug, Clone, Error)]
#[error(
    "pairing `{key}` on instance `{owner_instance_id}`: target `{target_instance_id}` \
     (deploys `{producer_name}:{producer_tag}`) has no available slot complementary to \
     `{pairing_name}:{pairing_tag}` role `{role}`"
)]
pub struct PairingTargetNotComplementary {
    pub owner_instance_id: String,
    pub key: String,
    pub target_instance_id: String,
    pub producer_name: String,
    pub producer_tag: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub role: String,
}

/// Payload for [`ParsingError::PairingTargetAmbiguous`]. The target instance
/// has more than one complementary unclaimed slot and the pair spec did not
/// name one.
#[derive(Debug, Clone, Error)]
#[error(
    "pairing `{key}` on instance `{owner_instance_id}`: target `{target_instance_id}` \
     has multiple complementary slots for `{pairing_name}:{pairing_tag}` \
     ([{candidate_link_ids}]) — disambiguate with `{key}@{target_instance_id}/<peer_link_id>`"
)]
pub struct PairingTargetAmbiguous {
    pub owner_instance_id: String,
    pub key: String,
    pub target_instance_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub candidate_link_ids: String,
}

/// Payload for [`ParsingError::PairingSlotAlreadyPaired`]. A slot (instance ×
/// link_id) was claimed by more than one pair in the same plan, or is already
/// paired in the running stack.
#[derive(Debug, Clone, Error)]
#[error(
    "pairing slot `{link_id}` of instance `{instance_id}` is already paired \
     (with `{existing_peer}`); a pairing slot is exclusive until cleared"
)]
pub struct PairingSlotAlreadyPaired {
    pub instance_id: String,
    pub link_id: String,
    pub existing_peer: String,
}

/// Payload for [`ParsingError::PairingConflict`]. Both endpoints declared the
/// pair but their declarations disagree.
#[derive(Debug, Clone, Error)]
#[error(
    "conflicting pairing declarations: `{instance_a}` pairs slot `{link_a}` with \
     `{target_a}`, but `{instance_b}` pairs slot `{link_b}` with `{target_b}` — \
     declaring a pair from both sides is allowed only when both agree"
)]
pub struct PairingConflict {
    pub instance_a: String,
    pub link_a: String,
    pub target_a: String,
    pub instance_b: String,
    pub link_b: String,
    pub target_b: String,
}

/// Payload for [`ParsingError::PairingSha256Mismatch`]. Both sides pin a
/// sha256 for the same pairing document and the pins differ.
#[derive(Debug, Clone, Error)]
#[error(
    "pairing `{pairing_name}:{pairing_tag}`: `{instance_a}` pins sha256 `{sha_a}` but \
     `{instance_b}` pins `{sha_b}` — both sides of a pair must reference the same document"
)]
pub struct PairingSha256Mismatch {
    pub instance_a: String,
    pub sha_a: String,
    pub instance_b: String,
    pub sha_b: String,
    pub pairing_name: String,
    pub pairing_tag: String,
}

/// Payload for [`ParsingError::PairingSlotUncovered`]. A declared pairing slot
/// was neither paired nor declared vacant. Forgetting a slot is an error
/// whether or not the manifest declares it optional: an optional slot may run
/// empty, but only where the deployment says so.
///
/// `Display` is hand-written rather than derived because the remedies it offers
/// follow `optional`: only a slot the manifest declares optional may be
/// vacated, so only its message names the vacancy spelling.
#[derive(Debug, Clone)]
pub struct PairingSlotUncovered {
    pub instance_id: String,
    pub link_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub role: String,
    /// Whether the node's manifest declares this slot `optional: true`.
    pub optional: bool,
}

impl std::fmt::Display for PairingSlotUncovered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            instance_id,
            link_id,
            pairing_name,
            pairing_tag,
            role,
            optional,
        } = self;
        let (kind, fix) = if *optional {
            ("optional", format!("or {}", vacancy_hint(link_id)))
        } else {
            (
                "required",
                emptiable_hint(link_id, PARTICIPANT_EMPTIABLE_KEY),
            )
        };
        write!(
            f,
            "instance `{instance_id}` declares {kind} pairing slot `{link_id}` (pairing \
             `{pairing_name}:{pairing_tag}`, role `{role}`) with no pair. Pair it (launcher \
             `links: {{ {link_id}: \"<peer_instance>\" }}` / \
             `--link {link_id}@<peer_instance>`), {fix}"
        )
    }
}

impl std::error::Error for PairingSlotUncovered {}

/// Payload for [`ParsingError::ObservationTargetNotObservable`]. The observer
/// slot's source instance declares no participant slot playing the observed
/// role for the referenced pairing document. Mirror of
/// [`PairingTargetNotComplementary`] for the observer mechanism (an observer
/// taps a participant's role output, so the source must actually play that
/// role).
#[derive(Debug, Clone, Error)]
#[error(
    "observer link `{key}` on instance `{owner_instance_id}`: source `{source_instance_id}` \
     (deploys `{source_name}:{source_tag}`) declares no participant slot playing role \
     `{observed_role}` for pairing `{pairing_name}:{pairing_tag}`"
)]
pub struct ObservationTargetNotObservable {
    pub owner_instance_id: String,
    pub key: String,
    pub source_instance_id: String,
    pub source_name: String,
    pub source_tag: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub observed_role: String,
}

/// Payload for [`ParsingError::ObservationTargetAmbiguous`]. The observer's
/// source instance plays the observed role through more than one participant
/// slot and the observer spec did not name one. Mirror of
/// [`PairingTargetAmbiguous`]; unlike pairing there is no exclusivity, so
/// every candidate is available, but the observer must still pick which
/// producer-side link_id to pin.
#[derive(Debug, Clone, Error)]
#[error(
    "observer link `{key}` on instance `{owner_instance_id}`: source `{source_instance_id}` \
     plays role `{observed_role}` for `{pairing_name}:{pairing_tag}` through multiple slots \
     ([{candidate_link_ids}]) — disambiguate with `{key}@{source_instance_id}/<source_link_id>`"
)]
pub struct ObservationTargetAmbiguous {
    pub owner_instance_id: String,
    pub key: String,
    pub source_instance_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub observed_role: String,
    pub candidate_link_ids: String,
}

/// Payload for [`ParsingError::ObservationSlotUncovered`]. A declared `one`,
/// `zero_or_one` or `one_or_more` observer slot was neither linked to a source
/// nor declared vacant. Mirror of [`PairingSlotUncovered`] for observers. A
/// `zero_or_more` slot never raises this: omitting it IS its empty set.
///
/// `Display` is hand-written rather than derived because the launcher link shape
/// it suggests follows the slot's cardinality, the way the binding errors'
/// suggestions do.
#[derive(Debug, Clone)]
pub struct ObservationSlotUncovered {
    pub instance_id: String,
    pub link_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub observed_role: String,
    pub cardinality: Cardinality,
}

impl std::fmt::Display for ObservationSlotUncovered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            instance_id,
            link_id,
            pairing_name,
            pairing_tag,
            observed_role,
            cardinality,
        } = self;
        // A scalar slot takes a single source; a `one_or_more` slot takes an
        // array. Only `zero_or_one` may say "nothing here", and it says it with
        // a vacancy: `one` observes exactly one pairing and `one_or_more` at
        // least one, so both take a source and nothing else.
        let link_shape = if cardinality.is_scalar() {
            format!("links: {{ {link_id}: \"<source_instance>\" }}")
        } else {
            format!("links: {{ {link_id}: [\"<source_instance>\", ...] }}")
        };
        let fix = match cardinality {
            Cardinality::ZeroOrOne => format!("or {}", vacancy_hint(link_id)),
            _ => emptiable_hint(link_id, OBSERVER_EMPTIABLE_KEY),
        };
        write!(
            f,
            "instance `{instance_id}` declares observer slot `{link_id}` (cardinality \
             `{cardinality}`, observes role `{observed_role}` of pairing \
             `{pairing_name}:{pairing_tag}`) with no source. Link it (launcher `{link_shape}` / \
             `--link {link_id}@<source_instance>`), {fix}"
        )
    }
}

impl std::error::Error for ObservationSlotUncovered {}

#[derive(Debug, Error, Clone)]
pub enum ParsingError {
    // -- General yaml syntax
    #[error("Cannot read {0}: {1}")]
    CannotRead(String, std::io::ErrorKind),
    #[error("Cannot parse configuration: {0}")]
    CannotParseConfig(String),
    #[error("Empty content found in: {0}")]
    EmptyContent(String),

    // -- launcher instances
    #[error("Duplicate name: {0}")]
    DuplicateName(String),

    // -- deployments
    #[error("Invalid deployment source: {0}")]
    InvalidDeploymentSource(String),

    // -- launcher: links
    #[error(
        "link `{link}` on instance `{owner_instance_id}` refers to unknown instance_id `{instance_id}`"
    )]
    UnknownInstanceId {
        owner_instance_id: String,
        link: String,
        instance_id: String,
    },
    #[error(
        "link key `{link}` on instance `{owner_instance_id}` is the reserved producer-default sentinel and cannot be used as a link id"
    )]
    LinkSentinelKey {
        owner_instance_id: String,
        link: String,
    },
    /// A `--link KEY@VALUE` / `links:` key that names no declared slot across
    /// any family (`depends_on.{nodes,contracts,pairings}`, participant or
    /// observer). The unified replacement for the old per-mechanism
    /// "unknown binding slot" / "dead pairing key" errors.
    #[error(transparent)]
    LinkUnknownSlot(Box<LinkUnknownSlot>),
    /// A `links:` value on a participant pairing slot arrived in array (or
    /// repeated-flag) shape. A pairing is strictly 1:1, so a participant slot
    /// takes exactly one target (`<peer_instance>[/<link_id>]`), never a set.
    #[error(
        "link `{link}` on instance `{owner_instance_id}` names a participant pairing slot but \
         its value is an array; a pairing is strictly 1:1, so the slot takes a single \
         `<instance>[/<link_id>]` target"
    )]
    LinkTargetNotScalar {
        owner_instance_id: String,
        link: String,
    },
    /// A declared `one` / `one_or_more` slot with no binding entry. Only a
    /// `zero_or_more` slot may be left unbound; there is no wildcard
    /// fallback for the others. Boxed for the same `result_large_err`
    /// reason as the other binding variants.
    #[error(transparent)]
    BindingSlotUnfulfilled(Box<BindingSlotUnfulfilled>),
    /// A launch-file array value (of any length, single-element and empty
    /// included) on a `cardinality: "one"` slot. The binding value's shape
    /// mirrors the slot's declared cardinality: a `one` slot takes a scalar
    /// string only.
    #[error(
        "binding `{binding}` on instance `{owner_instance_id}` is an array, but the slot's \
         cardinality is `one`: bind a scalar (`{binding}: \"<producer_instance_id>\"`), or \
         declare `cardinality: \"one_or_more\"` / `\"zero_or_more\"` on the dependency to \
         accept a producer array"
    )]
    BindingArrayOnOneSlot {
        owner_instance_id: String,
        binding: String,
    },
    /// A launch-file scalar value on a `one_or_more` / `zero_or_more` slot.
    /// Multi-cardinality slots take an array of instance ids, even for a
    /// single producer.
    #[error(
        "binding `{binding}` on instance `{owner_instance_id}` is a scalar, but the slot's \
         cardinality is `{cardinality}`: bind an array \
         (`{binding}: [\"<producer_instance_id>\", ...]`)"
    )]
    BindingScalarOnMultiSlot {
        owner_instance_id: String,
        binding: String,
        cardinality: Cardinality,
    },
    /// An empty array on a `one_or_more` slot: the binding entry exists but
    /// its set does not meet the slot's minimum of one producer. (An empty
    /// array is a valid definition only for `zero_or_more`.)
    #[error(
        "binding `{binding}` on instance `{owner_instance_id}` is an empty array, but the \
         slot's cardinality is `one_or_more`: bind at least one producer, or declare \
         `cardinality: \"zero_or_more\"` on the dependency if an empty set is meaningful"
    )]
    BindingCardinalityUnmet {
        owner_instance_id: String,
        binding: String,
    },
    /// Repeated `--link KEY@…` occurrences on a `cardinality: "one"` slot.
    /// Flag repetition accumulates a multi slot's set and stays a hard
    /// error on a `one` slot.
    #[error(
        "{target_count} `--link {binding}@…` occurrences on instance `{owner_instance_id}`, \
         but the slot's cardinality is `one`: pass exactly one, or declare \
         `cardinality: \"one_or_more\"` / `\"zero_or_more\"` on the dependency"
    )]
    BindingSingleSlotMultipleTargets {
        owner_instance_id: String,
        binding: String,
        target_count: usize,
    },
    /// Boxed payload so this variant does not grow `ParsingError` past the
    /// `clippy::result_large_err` threshold; without the indirection, the
    /// seven `String` fields would inflate every `Result<_, _>` that
    /// transitively wraps a `ParsingError` (notably code generated against
    /// `peppylib::PeppyError`).
    #[error(transparent)]
    BindingTargetMismatch(Box<BindingTargetMismatch>),
    /// A `--link` targets a contract slot but the producer doesn't
    /// declare the requested contract in `manifest.implements`. Boxed for
    /// the same `result_large_err` reason as the other binding variants.
    #[error(transparent)]
    BindingContractNotImplemented(Box<BindingContractNotImplemented>),
    /// Two instances anywhere in the running stack share an `instance_id`.
    /// Boxed for the same `result_large_err` reason as the other binding
    /// variants.
    #[error(transparent)]
    DuplicateInstanceIdAcrossStack(Box<DuplicateInstanceIdAcrossStack>),

    // -- launcher/CLI: pairings
    #[error(transparent)]
    PairingTargetNotComplementary(Box<PairingTargetNotComplementary>),
    #[error(transparent)]
    PairingTargetAmbiguous(Box<PairingTargetAmbiguous>),
    #[error(transparent)]
    PairingSlotAlreadyPaired(Box<PairingSlotAlreadyPaired>),
    #[error(transparent)]
    PairingConflict(Box<PairingConflict>),
    #[error(transparent)]
    PairingSha256Mismatch(Box<PairingSha256Mismatch>),
    #[error(transparent)]
    PairingSlotUncovered(Box<PairingSlotUncovered>),

    // -- launcher/CLI: observers
    #[error(transparent)]
    ObservationTargetNotObservable(Box<ObservationTargetNotObservable>),
    #[error(transparent)]
    ObservationTargetAmbiguous(Box<ObservationTargetAmbiguous>),
    #[error(transparent)]
    ObservationSlotUncovered(Box<ObservationSlotUncovered>),
    /// A launch-file array value (of any length) on a `cardinality: "one"`
    /// observer slot. An observer link's shape mirrors the slot's declared
    /// cardinality exactly as a producer binding's does: a `one` slot takes a
    /// single `<instance>[/<link_id>]` target.
    #[error(
        "link `{link}` on instance `{owner_instance_id}` is an array, but the observer slot's \
         cardinality is `{cardinality}`: link a single `<source_instance>[/<source_link_id>]` \
         target, or declare `cardinality: \"one_or_more\"` / `\"zero_or_more\"` on the observer \
         entry"
    )]
    ObservationArrayOnScalarSlot {
        owner_instance_id: String,
        link: String,
        cardinality: Cardinality,
    },
    /// A launch-file scalar value on a `one_or_more` / `zero_or_more` observer
    /// slot. Multi-cardinality slots take an array of targets, even for a
    /// single observed pairing.
    #[error(
        "link `{link}` on instance `{owner_instance_id}` is a scalar, but the observer slot's \
         cardinality is `{cardinality}`: link an array \
         (`{link}: [\"<source_instance>\", ...]`)"
    )]
    ObservationScalarOnMultiSlot {
        owner_instance_id: String,
        link: String,
        cardinality: Cardinality,
    },
    /// An empty array on a `one_or_more` observer slot: the link entry exists
    /// but its set does not meet the slot's minimum of one observed pairing.
    /// (An empty array is a valid definition only for `zero_or_more`.)
    #[error(
        "link `{link}` on instance `{owner_instance_id}` is an empty array, but the observer \
         slot's cardinality is `one_or_more`: link at least one source, or declare \
         `cardinality: \"zero_or_more\"` on the observer entry if observing nothing is \
         meaningful"
    )]
    ObservationCardinalityUnmet {
        owner_instance_id: String,
        link: String,
    },
    /// Repeated `--link KEY@…` occurrences on a scalar observer slot. Flag
    /// repetition accumulates a multi slot's set and stays a hard error on a
    /// `one` or `zero_or_one` slot.
    #[error(
        "{target_count} `--link {link}@…` occurrences on instance `{owner_instance_id}`, but the \
         observer slot's cardinality is `{cardinality}`: pass exactly one, or declare \
         `cardinality: \"one_or_more\"` / `\"zero_or_more\"` on the observer entry"
    )]
    ObservationSingleSlotMultipleTargets {
        owner_instance_id: String,
        link: String,
        cardinality: Cardinality,
        target_count: usize,
    },
    /// Two of an observer slot's targets resolved to the same observed pairing
    /// (same source instance, same source-side slot). Caught after resolution
    /// because distinct target strings can name one pairing: on a source with a
    /// single observable slot, `"dual_1"` and `"dual_1/left_ctl"` are the same
    /// member.
    #[error(
        "link `{link}` on instance `{owner_instance_id}` observes `{source_instance_id}/\
         {source_link_id}` twice: an observer slot observes each pairing once, so drop the \
         repeated target"
    )]
    DuplicateObservationTarget {
        owner_instance_id: String,
        link: String,
        source_instance_id: String,
        source_link_id: String,
    },

    /// A `{ vacant: "<why>" }` value on a slot whose manifest does not declare
    /// it emptiable, or one that already has a spelling for its empty state.
    /// A deployment decides that a specific instance runs a slot empty; the
    /// node decides whether any deployment may.
    #[error(
        "link `{link_id}` on instance `{owner_instance_id}` is {slot_kind} and cannot be \
         vacant. {refusal}"
    )]
    LinkVacantInvalid {
        owner_instance_id: String,
        link_id: String,
        slot_kind: String,
        refusal: VacancyRefusal,
    },
}

/// Why a slot refuses `{ vacant: "<why>" }`, and what to write instead. The
/// three arms are three different facts with three different remedies: the node
/// has not declared the slot emptiable, the slot has no empty state at all, or
/// it has one and spells it another way. Every payload is prose fixed by the
/// slot's kind, so the arms carry `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VacancyRefusal {
    /// The manifest declares the slot required, so no deployment may empty it.
    ManifestRequires {
        /// The manifest key that would make the slot emptiable, named so the
        /// error offers the node-side remedy alongside the deployment-side one.
        declare_optional: &'static str,
    },
    /// The slot cannot be empty in any spelling, so the only remedy is to fill
    /// it (or to declare a cardinality with a floor of zero).
    NoEmptyState {
        /// What the slot takes instead, and why it has no empty state.
        requirement: &'static str,
    },
    /// The slot may be empty, but writes it another way.
    SpelledDifferently { empty_spelling: &'static str },
}

impl std::fmt::Display for VacancyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VacancyRefusal::ManifestRequires { declare_optional } => write!(
                f,
                "The node manifest declares it required: link it, or declare {declare_optional}"
            ),
            VacancyRefusal::NoEmptyState { requirement } => {
                write!(f, "It has no empty state: it takes {requirement}")
            }
            VacancyRefusal::SpelledDifferently { empty_spelling } => {
                write!(f, "Its empty state is {empty_spelling}")
            }
        }
    }
}

/// How a coverage failure tells the reader to write the vacancy down, shared
/// by [`PairingSlotUncovered`] and [`ObservationSlotUncovered`] so the two
/// surfaces cannot drift on the spelling of a feature whose whole point is
/// that there is one spelling.
/// The manifest key that declares a participant pairing slot emptiable, spelled
/// once so the uncovered-slot remedy and [`VacancyRefusal::ManifestRequires`]
/// cannot drift on it.
pub(crate) const PARTICIPANT_EMPTIABLE_KEY: &str =
    "`optional: true` on its `depends_on.pairings` entry";

/// The manifest key that declares an observer slot emptiable, shared for the
/// same reason as [`PARTICIPANT_EMPTIABLE_KEY`].
pub(crate) const OBSERVER_EMPTIABLE_KEY: &str =
    "`cardinality: \"zero_or_one\"` on its `depends_on.pairing_observers` entry";

fn vacancy_hint(link_id: &str) -> String {
    format!(
        "leave it empty on purpose (`links: {{ {link_id}: {{ vacant: \"<why>\" }} }}` / \
         `--vacant-link '{link_id}=<why>'`)"
    )
}

/// The remedy for an uncovered slot whose node has not declared it emptiable.
/// It names BOTH steps, because the manifest key alone leaves the slot just as
/// uncovered: an emptiable slot still needs the deployment to write the vacancy
/// down. `declare_emptiable` names the key for this slot kind.
fn emptiable_hint(link_id: &str, declare_emptiable: &str) -> String {
    format!(
        "or, if it is meant to run empty, declare {declare_emptiable} and then {vacancy}",
        vacancy = vacancy_hint(link_id)
    )
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum StructuredError {
    InvalidDeploymentSource(String),
    DuplicateName(String),
    UnknownInstanceId {
        owner_instance_id: String,
        link: String,
        instance_id: String,
    },
    LinkSentinelKey {
        owner_instance_id: String,
        link: String,
    },
}

impl StructuredError {
    pub(crate) fn json5_message(&self) -> String {
        serde_json5::to_string(self).unwrap_or_else(|_| "serialization error".to_string())
    }
}

impl From<StructuredError> for ParsingError {
    fn from(s: StructuredError) -> Self {
        match s {
            StructuredError::InvalidDeploymentSource(detail) => {
                ParsingError::InvalidDeploymentSource(detail)
            }
            StructuredError::DuplicateName(id) => ParsingError::DuplicateName(id),
            StructuredError::UnknownInstanceId {
                owner_instance_id,
                link,
                instance_id,
            } => ParsingError::UnknownInstanceId {
                owner_instance_id,
                link,
                instance_id,
            },
            StructuredError::LinkSentinelKey {
                owner_instance_id,
                link,
            } => ParsingError::LinkSentinelKey {
                owner_instance_id,
                link,
            },
        }
    }
}

impl From<serde_json5::Error> for ParsingError {
    fn from(err: serde_json5::Error) -> Self {
        match err {
            serde_json5::Error::Message { msg, .. } => {
                if let Ok(structured) = serde_json5::from_str::<StructuredError>(&msg) {
                    ParsingError::from(structured)
                } else {
                    ParsingError::CannotParseConfig(msg)
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // -- Parsing error
    #[error(transparent)]
    Parsing(#[from] ParsingError),
    #[error("Serialize error: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_structured_error_deserialization() {
        // Helper to create a serde_json5 error from a string
        fn make_err(msg: &str) -> serde_json5::Error {
            serde::de::Error::custom(msg)
        }

        // InvalidDeploymentSource
        let json = serde_json5::to_string(&StructuredError::InvalidDeploymentSource(
            "bad source".to_string(),
        ))
        .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::InvalidDeploymentSource(msg) = err {
            assert_eq!(msg, "bad source");
        } else {
            panic!("Expected InvalidDeploymentSource, got {:?}", err);
        }

        // DuplicateName
        let json =
            serde_json5::to_string(&StructuredError::DuplicateName("id1".to_string())).unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::DuplicateName(id) = err {
            assert_eq!(id, "id1");
        } else {
            panic!("Expected DuplicateName, got {:?}", err);
        }
    }

    #[test]
    fn test_fallback_mechanism() {
        fn make_err(msg: &str) -> serde_json5::Error {
            serde::de::Error::custom(msg)
        }

        let raw_msg = "This is not JSON";
        let err = ParsingError::from(make_err(raw_msg));
        if let ParsingError::CannotParseConfig(msg) = err {
            assert_eq!(msg, raw_msg);
        } else {
            panic!("Expected CannotParseConfig, got {:?}", err);
        }

        let broken_json = "{ invalid json";
        let err = ParsingError::from(make_err(broken_json));
        if let ParsingError::CannotParseConfig(msg) = err {
            assert_eq!(msg, broken_json);
        } else {
            panic!("Expected CannotParseConfig, got {:?}", err);
        }
    }

    #[test]
    fn format_bulleted_empty_input_is_empty_string() {
        let out = format_bulleted(Vec::<String>::new());
        assert_eq!(out, "");
    }

    #[test]
    fn format_bulleted_prefixes_each_item_with_a_newline_bullet() {
        let out = format_bulleted(["first", "second"]);
        // Each item gets a leading "\n  - "; nothing is emitted around the list,
        // so it can be spliced straight into a parent diagnostic string.
        assert_eq!(out, "\n  - first\n  - second");
    }

    #[test]
    fn format_bulleted_accepts_any_display_type() {
        // Exercises the generic `T: Display` bound with a non-string item.
        let out = format_bulleted(1..=3);
        assert_eq!(out, "\n  - 1\n  - 2\n  - 3");
    }
}
