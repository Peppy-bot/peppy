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
/// `result_large_err` reason as the other binding variants — the six
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

/// Payload for [`ParsingError::MissingInterface`]. Boxed in the variant so
/// the six `String` fields do not inflate `ParsingError` past the
/// `clippy::result_large_err` threshold.
#[derive(Debug, Clone, Error)]
#[error(
    "`{dependant}`:{dependant_tag} expects {interface_kind} `{interface_name}` from \
     `{dependency}`:{dependency_tag}, but it is not exposed"
)]
pub struct MissingInterface {
    pub dependant: String,
    pub dependant_tag: String,
    pub dependency: String,
    pub dependency_tag: String,
    pub interface_kind: String,
    pub interface_name: String,
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

    // -- node_config
    #[error("Invalid name: {0}, allowed characters: {1}")]
    InvalidName(String, String),
    #[error("Empty name")]
    EmptyName,
    #[error("Duplicate name: {0}")]
    DuplicateName(String),
    #[error(
        "Duplicate link_id `{0}` in manifest.depends_on (link_ids must be unique across nodes and interfaces)"
    )]
    DuplicateLinkId(String),
    #[error(
        "Conflicting `from_any: true` for dependency `{name}` (tag `{tag}`) in manifest.depends_on: only one entry per (name, tag) may set from_any=true"
    )]
    ConflictingFromAny { name: String, tag: String },

    // -- deployments
    #[error("Invalid deployment source: {0}")]
    InvalidDeploymentSource(String),

    // -- build system
    #[error("Invalid toolchain {0}")]
    InvalidToolchain(String),

    // -- node config: process vs container
    #[error("Node config must have exactly one of `process` or `container`, not both")]
    ProcessAndContainerConflict,
    #[error("Node config must have either `process` or `container`")]
    NoProcessOrContainer,
    #[error("Node config `execution.run_cmd` must not be empty")]
    EmptyRunCmd,

    // -- node config: execution
    #[error("Node config `execution.language` is required when an execution block is defined")]
    MissingExecutionLanguage,

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
    /// `KEY`. Each `KEY` is the binding's label — pinned KEYs match a
    /// declared link_id; `from_any` KEYs are free-form — and must be
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

    // -- container config: mount paths
    #[error(
        "Invalid mount path `{0}`: top-level system directories ({1}) cannot be used as mount sources — use a subdirectory instead (e.g., /tmp/my_app)"
    )]
    InvalidMountPath(String, String),
    #[error("Invalid parameter reference `${{parameters:{0}}}` in mount path: {1}")]
    InvalidMountPathParameterRef(String, String),

    // -- node dependency validation
    #[error(
        "`{dependant}:{dependant_tag}` depends on `{dependency}:{dependency_tag}`, but it does not exist in the stack"
    )]
    MissingDependency {
        dependant: String,
        dependant_tag: String,
        dependency: String,
        dependency_tag: String,
    },
    #[error(
        "`{dependant}:{dependant_tag}` references undeclared link_id `{link_id}` in consumed interfaces"
    )]
    UndeclaredLinkId {
        dependant: String,
        dependant_tag: String,
        link_id: String,
    },
    /// Boxed payload for the same reason as
    /// [`ParsingError::BindingTargetMismatch`]: keeps the variant's
    /// String-heavy struct from inflating `ParsingError`'s size past the
    /// `clippy::result_large_err` threshold.
    #[error(transparent)]
    MissingInterface(Box<MissingInterface>),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum StructuredError {
    InvalidDeploymentSource(String),
    DuplicateName(String),
    InvalidName {
        name: String,
        allowed: String,
    },
    EmptyName,
    MissingExecutionLanguage,
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
            StructuredError::InvalidName { name, allowed } => {
                ParsingError::InvalidName(name, allowed)
            }
            StructuredError::EmptyName => ParsingError::EmptyName,
            StructuredError::MissingExecutionLanguage => ParsingError::MissingExecutionLanguage,
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
    #[error("Capnp error: {0}")]
    Capnp(#[from] capnp::Error),

    // -- Parsing error
    #[error(transparent)]
    Parsing(#[from] ParsingError),
    #[error("Serialize error: {0}")]
    Serialize(String),
    #[error("Encoding error: {0}")]
    Encoding(String),

    // -- Fingerprint
    #[error(
        "Node config fingerprint mismatch: expected {expected}, got {actual}. The config may have been modified after code generation. Run `node sync` to update the peppygen lib on your node."
    )]
    FingerprintMismatch { expected: String, actual: String },
    #[error(
        "Release fingerprint mismatch: node was generated with peppy version {node_version}, but current peppy version is {current_version}. Run `node sync` to regenerate with the current version."
    )]
    ReleaseFingerprintMismatch {
        node_version: String,
        current_version: String,
    },
    #[error("Release fingerprint missing: {0}")]
    ReleaseFingerprintMissing(String),
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

        // InvalidName
        let json = serde_json5::to_string(&StructuredError::InvalidName {
            name: "bad".to_string(),
            allowed: "a-z".to_string(),
        })
        .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::InvalidName(name, allowed) = err {
            assert_eq!(name, "bad");
            assert_eq!(allowed, "a-z");
        } else {
            panic!("Expected InvalidName, got {:?}", err);
        }

        // EmptyName
        let json = serde_json5::to_string(&StructuredError::EmptyName).unwrap();
        let err = ParsingError::from(make_err(&json));
        if !matches!(err, ParsingError::EmptyName) {
            panic!("Expected EmptyName, got {:?}", err);
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
