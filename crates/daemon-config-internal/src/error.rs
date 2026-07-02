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
/// or an interface dep (matched against the producer's `conforms_to`). Used
/// in error payloads so messages can name the expected category in singular
/// human form instead of leaking the `depends_on.nodes` / `depends_on.interfaces`
/// field path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Node,
    Interface,
}

impl core::fmt::Display for SlotKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            SlotKind::Node => "node",
            SlotKind::Interface => "interface",
        })
    }
}

/// Payload for [`ParsingError::BindingMissingForPinnedDep`]. Boxed in the
/// variant so the five `String` fields do not inflate `ParsingError` past
/// the `clippy::result_large_err` threshold.
#[derive(Debug, Clone, Error)]
#[error(
    "instance `{owner_instance_id}`: slot `{link_id}` is unbound \
     (expected {kind} `{expected_name}:{expected_tag}`)"
)]
pub struct BindingMissingForPinnedDep {
    pub owner_instance_id: String,
    pub link_id: String,
    pub kind: SlotKind,
    pub expected_name: String,
    pub expected_tag: String,
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

/// Payload for [`ParsingError::BindingInterfaceNotConformed`]. Raised when a
/// `--bind` targets an interface slot but the producer's `interfaces.conforms_to`
/// list does not include the requested `(interface_name, interface_tag)`.
///
/// Boxed in the variant for the same `clippy::result_large_err` reason as the
/// other binding error payloads.
#[derive(Debug, Clone, Error)]
#[error(
    "binding `{binding}` on instance `{owner_instance_id}`: target \
     `{target_instance_id}` deploys `{producer_name}:{producer_tag}`, but \
     the slot requires interface `{interface_name}:{interface_tag}` (add it \
     to the producer's `conforms_to`)"
)]
pub struct BindingInterfaceNotConformed {
    pub owner_instance_id: String,
    pub binding: String,
    pub target_instance_id: String,
    pub interface_name: String,
    pub interface_tag: String,
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
/// `--bind KEY@id` ambiguous.
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

/// Payload for [`ParsingError::BindingDeadKey`]. Boxed for the same
/// `result_large_err` reason as the other binding variants; the six
/// `String` fields push the enum past the lint threshold otherwise.
#[derive(Debug, Clone, Error)]
#[error(
    "binding `{binding}` on instance `{owner_instance_id}` matches no \
     declared slot, and no `from_any` slot accepts target \
     `{target_instance_id}` (deploys `{producer_name}:{producer_tag}`); \
     declared link_ids: [{declared_link_ids}]"
)]
pub struct BindingDeadKey {
    pub owner_instance_id: String,
    pub binding: String,
    pub target_instance_id: String,
    pub producer_name: String,
    pub producer_tag: String,
    pub declared_link_ids: String,
}

/// Payload for [`ParsingError::PairingDeadKey`]. A `pairings:` key (or
/// `--pair` link_id) that matches no declared `depends_on.pairings` slot of
/// the instance's node.
#[derive(Debug, Clone, Error)]
#[error(
    "pairing key `{key}` on instance `{owner_instance_id}` matches no declared \
     pairing slot; declared pairing link_ids: [{declared_link_ids}]"
)]
pub struct PairingDeadKey {
    pub owner_instance_id: String,
    pub key: String,
    pub declared_link_ids: String,
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

/// Payload for [`ParsingError::PairingSlotUncovered`]. A required pairing
/// slot was neither paired nor explicitly deferred.
#[derive(Debug, Clone, Error)]
#[error(
    "instance `{instance_id}` declares required pairing slot `{link_id}` (pairing \
     `{pairing_name}:{pairing_tag}`, role `{role}`) with no pair. Pair it (launcher \
     `pairings: {{ {link_id}: \"<peer_instance>\" }}` / `--pair {link_id}@<peer_instance>`), \
     or defer it deliberately (`defer_pairings: [\"{link_id}\"]` / `--defer-pair {link_id}`)"
)]
pub struct PairingSlotUncovered {
    pub instance_id: String,
    pub link_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
    pub role: String,
}

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

    // -- launcher: interface bindings
    #[error(
        "interface binding `{binding}` on instance `{owner_instance_id}` refers to unknown instance_id `{instance_id}`"
    )]
    UnknownInstanceId {
        owner_instance_id: String,
        binding: String,
        instance_id: String,
    },
    #[error(
        "binding key `{binding}` on instance `{owner_instance_id}` is the reserved producer-default sentinel and cannot be used as a binding slot"
    )]
    BindingSentinelKey {
        owner_instance_id: String,
        binding: String,
    },
    /// `--bind KEY@VALUE` whose `KEY` neither matches a declared pinned
    /// `link_id` nor a declared `from_any` slot for VALUE's `(name, tag)`.
    /// Boxed for the same `result_large_err` reason as the other binding
    /// variants.
    #[error(transparent)]
    BindingDeadKey(Box<BindingDeadKey>),
    /// Two `--bind KEY@…` entries on the same invocation share the same
    /// `KEY`. Each `KEY` is the binding's label (pinned KEYs match a
    /// declared link_id; `from_any` KEYs are free-form) and must be
    /// distinct so the validator can resolve each to a slot
    /// unambiguously.
    #[error(
        "duplicate binding key `{binding}` on instance `{owner_instance_id}` (each --bind KEY must be distinct)"
    )]
    BindingDuplicateKey {
        owner_instance_id: String,
        binding: String,
    },
    /// Boxed payload for the same reason as
    /// [`ParsingError::BindingTargetMismatch`]: keeps the variant's
    /// String-heavy struct from inflating `ParsingError`'s size past the
    /// `clippy::result_large_err` threshold.
    #[error(transparent)]
    BindingMissingForPinnedDep(Box<BindingMissingForPinnedDep>),
    /// Boxed payload so this variant does not grow `ParsingError` past the
    /// `clippy::result_large_err` threshold; without the indirection, the
    /// seven `String` fields would inflate every `Result<_, _>` that
    /// transitively wraps a `ParsingError` (notably code generated against
    /// `peppylib::PeppyError`).
    #[error(transparent)]
    BindingTargetMismatch(Box<BindingTargetMismatch>),
    /// Pinned `--bind` targets an interface slot but the producer doesn't
    /// declare conformance to the requested interface. Boxed for the same
    /// `result_large_err` reason as the other binding variants.
    #[error(transparent)]
    BindingInterfaceNotConformed(Box<BindingInterfaceNotConformed>),
    /// Two instances anywhere in the running stack share an `instance_id`.
    /// Boxed for the same `result_large_err` reason as the other binding
    /// variants.
    #[error(transparent)]
    DuplicateInstanceIdAcrossStack(Box<DuplicateInstanceIdAcrossStack>),

    // -- launcher/CLI: pairings
    /// A `--bind`/`bindings:` key that names a pairing slot. Pairing slots
    /// are never binding slots; the establishment surface is `--pair` /
    /// launcher `pairings:`.
    #[error(
        "binding key `{binding}` on instance `{owner_instance_id}` names a pairing slot; \
         pair it with `--pair {binding}@<peer_instance>` (or launcher `pairings:`) instead of --bind"
    )]
    BindingKeyIsPairingSlot {
        owner_instance_id: String,
        binding: String,
    },
    #[error(transparent)]
    PairingDeadKey(Box<PairingDeadKey>),
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
    /// A `defer_pairings` entry that is invalid: it names an unknown slot,
    /// an `optional: true` slot (deferring is redundant — optional slots
    /// boot unpaired without ceremony), or a slot that is also paired in
    /// the same plan.
    #[error(
        "defer_pairings entry `{link_id}` on instance `{owner_instance_id}` is invalid: {reason}"
    )]
    PairingDeferInvalid {
        owner_instance_id: String,
        link_id: String,
        reason: String,
    },
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum StructuredError {
    InvalidDeploymentSource(String),
    DuplicateName(String),
    UnknownInstanceId {
        owner_instance_id: String,
        binding: String,
        instance_id: String,
    },
    BindingSentinelKey {
        owner_instance_id: String,
        binding: String,
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
                binding,
                instance_id,
            } => ParsingError::UnknownInstanceId {
                owner_instance_id,
                binding,
                instance_id,
            },
            StructuredError::BindingSentinelKey {
                owner_instance_id,
                binding,
            } => ParsingError::BindingSentinelKey {
                owner_instance_id,
                binding,
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
