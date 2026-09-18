use super::types::{DependsOn, Execution, InterfaceKind, Interfaces, Manifest, NodeConfig};
use crate::{
    consts::normalize_tag,
    error::{ParsingError, Result},
    parsing::read_non_empty_file,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Validates the manifest's link namespace and implements claims:
///
/// - Rejects duplicate `link_id`s across the combined
///   nodes/contracts/pairings/implements set. `link_id` is the shared
///   identity for all four: consumed topics/services/actions resolve their
///   producer by `link_id` alone, produced entries resolve their implements
///   slot the same way, and pairing slots are addressed by `link_id` in
///   `--pair`/`pairings:`, so a collision either silently overwrites a
///   resolution or binds the wrong slot.
/// - Rejects pairing link_ids that are not wire-safe segments. A participant
///   slot link_id is stamped verbatim into the producer-side link_id segment
///   of every publish keyexpr, so it must be wire-safe. An observer link_id is
///   a local resolution name only (never stamped on the wire), but it is held
///   to the same rule so both pairing forms share one parser.
/// - Rejects duplicate `(name, tag)` pairs in `manifest.implements`
///   (including tag-sanitization collisions like `v1` vs `v-1`): wire keys
///   embed only `contract/{name}/{tag}` plus the `_` sentinel link slot, so
///   two instances of one contract in one node would collide on the wire.
fn validate_manifest_links(manifest: &Manifest) -> Result<()> {
    let mut seen_link_ids: HashSet<&str> = HashSet::new();
    let implements = manifest.implements.iter().map(|e| e.link_id.as_str());
    let depends = manifest.depends_on.iter().flat_map(|d| {
        d.nodes
            .iter()
            .map(|n| n.link_id.as_str())
            .chain(d.contracts.iter().map(|c| c.link_id.as_str()))
            .chain(d.pairing_link_ids())
    });
    for link_id in depends.chain(implements) {
        if !seen_link_ids.insert(link_id) {
            return Err(ParsingError::DuplicateLinkId(link_id.to_owned()).into());
        }
    }

    if let Some(depends_on) = &manifest.depends_on {
        for link_id in depends_on.pairing_link_ids() {
            if !is_wire_safe_link_id(link_id) {
                return Err(ParsingError::PairingSentinelLinkId(link_id.to_owned()).into());
            }
        }
    }

    let mut seen_contracts: HashMap<(&str, String), &str> = HashMap::new();
    for entry in &manifest.implements {
        let key = (entry.name.as_str(), normalize_tag(&entry.tag));
        if let Some(prev_tag) = seen_contracts.insert(key, entry.tag.as_str()) {
            if prev_tag == entry.tag {
                return Err(ParsingError::DuplicateImplementsContract {
                    name: entry.name.as_str().to_owned(),
                    tag: entry.tag.clone(),
                }
                .into());
            }
            return Err(ParsingError::ImplementsTagSanitizationCollision {
                name: entry.name.as_str().to_owned(),
                tag_a: prev_tag.to_owned(),
                tag_b: entry.tag.clone(),
            }
            .into());
        }
    }
    Ok(())
}

/// Validates the link_id direction rules and duplicate keys of the
/// produced/consumed interface entries:
///
/// - A `topics.emits` entry's `link_id` must name a `manifest.implements`
///   slot or a participant `depends_on.pairings` slot: a node declares the
///   pairing topics its role emits the same way a contract implementer
///   declares contract members. An observer slot plays no role and emits
///   nothing, so naming one gets a targeted error. `services.exposes` and
///   `actions.exposes` admit implements slots only, because a pairing
///   document declares topics only. Naming any other `depends_on` slot, or
///   nothing at all, gets a dedicated error each.
/// - A consumed entry's `link_id` must not name an implements slot (those
///   are produced, not consumed); resolution against `depends_on` stays in
///   `validate_dependency_specs`, which runs where dependencies resolve.
/// - Native entries are unique by `name` per section; document-backed
///   entries are unique by `(link_id, name)` per section. A native and a
///   document-backed entry may share a name (they are namespaced apart in
///   modules, schema keys, and wire keys).
///
/// Membership of a pairing-backed entry in its document, and per-slot emit
/// coverage, need the pairing document and are checked at node add/sync.
fn validate_interfaces(manifest: &Manifest, interfaces: &Interfaces) -> Result<()> {
    const KINDS: [(InterfaceKind, &str); 3] = [
        (InterfaceKind::Topic, crate::node::EmittedTopic::SECTION),
        (InterfaceKind::Service, crate::node::ExposedService::SECTION),
        (InterfaceKind::Action, crate::node::ExposedAction::SECTION),
    ];

    let implements_link_ids: HashSet<&str> = manifest
        .implements
        .iter()
        .map(|e| e.link_id.as_str())
        .collect();
    let mut topic_emit_link_ids = implements_link_ids.clone();
    topic_emit_link_ids.extend(participant_pairing_link_ids(manifest));

    // An observer plays no role and emits nothing, which is knowable from the
    // manifest alone, so a `topics.emits` entry naming an observer slot is
    // rejected here with a targeted error (the observed role's topics belong
    // in `topics.consumes`). A linear scan of the (tiny) slot list, like
    // `depends_list_containing`.
    let observer_link_ids: Vec<&str> = manifest
        .depends_on
        .iter()
        .flat_map(|d| d.pairing_observers.iter())
        .map(|o| o.link_id.as_str())
        .collect();
    for (link_id, _) in interfaces.produced(InterfaceKind::Topic) {
        if let Some(link_id) = link_id
            && observer_link_ids.contains(&link_id)
        {
            return Err(ParsingError::EmitsOnObserverSlot {
                link_id: link_id.to_owned(),
            }
            .into());
        }
    }

    for (kind, section) in KINDS {
        let allowed = match kind {
            InterfaceKind::Topic => &topic_emit_link_ids,
            InterfaceKind::Service | InterfaceKind::Action => &implements_link_ids,
        };
        validate_produced_section(
            section,
            interfaces.produced(kind),
            allowed,
            manifest.depends_on.as_ref(),
        )?;
    }

    for (kind, _) in KINDS {
        for (link_id, name, refines) in interfaces.consumed(kind) {
            if implements_link_ids.contains(link_id) {
                return Err(ParsingError::ConsumedItemReferencesImplementsLinkId {
                    link_id: link_id.to_owned(),
                }
                .into());
            }
            // `refine` pins arrays a document leaves generic, so it needs a
            // document behind the slot: a consumed entry naming a
            // `depends_on.nodes` producer takes that producer's native shape
            // as is.
            if refines
                && depends_list_containing(manifest.depends_on.as_ref(), link_id) == Some("nodes")
            {
                return Err(ParsingError::RefineOnNodeSlot {
                    section: kind.consumed_section().to_owned(),
                    link_id: link_id.to_owned(),
                    name: name.to_owned(),
                }
                .into());
            }
        }
    }

    // An observer slot that nothing consumes resolves a source and taps
    // nothing, which is always a mistake. A participant slot may consume
    // nothing legitimately (it still emits its role's topics), so this rule
    // applies to observers only.
    let consumed_topic_link_ids: HashSet<&str> = interfaces
        .consumed(InterfaceKind::Topic)
        .map(|(link_id, _, _)| link_id)
        .collect();
    for link_id in &observer_link_ids {
        if !consumed_topic_link_ids.contains(link_id) {
            return Err(ParsingError::ObserverLinkNeverConsumed {
                link_id: (*link_id).to_owned(),
            }
            .into());
        }
    }

    Ok(())
}

/// Which `depends_on` list (if any) declares `link_id`. Only consulted on
/// the error path, so a linear scan of the (tiny) lists beats prebuilding
/// a lookup map that valid configs never read.
fn depends_list_containing(depends_on: Option<&DependsOn>, link_id: &str) -> Option<&'static str> {
    let d = depends_on?;
    if d.nodes.iter().any(|n| n.link_id == link_id) {
        return Some("nodes");
    }
    if d.contracts.iter().any(|c| c.link_id == link_id) {
        return Some("contracts");
    }
    if d.pairings.iter().any(|p| p.link_id == link_id) {
        return Some("pairings");
    }
    if d.pairing_observers.iter().any(|o| o.link_id == link_id) {
        return Some("pairing_observers");
    }
    None
}

/// The `depends_on.pairings` slot link_ids of a manifest. A participant slot is
/// the one `depends_on` kind a produced topic entry may reference: its role is
/// a producer of the pairing document's topics. An observer plays no role and
/// emits nothing, so `depends_on.pairing_observers` is excluded and a
/// `topics.emits` entry naming one is rejected with a targeted error. Which
/// topics a participant's role actually emits needs the pairing document and is
/// enforced at node add/sync.
fn participant_pairing_link_ids(manifest: &Manifest) -> impl Iterator<Item = &str> {
    manifest
        .depends_on
        .iter()
        .flat_map(|d| d.pairings.iter())
        .map(|p| p.link_id.as_str())
}

/// `allowed_link_ids` is the set a document-backed entry of this section may
/// reference: `manifest.implements` slots, plus participant pairing slots
/// for `topics.emits`.
fn validate_produced_section<'a>(
    section: &'static str,
    entries: impl Iterator<Item = (Option<&'a str>, &'a str)>,
    allowed_link_ids: &HashSet<&str>,
    depends_on: Option<&DependsOn>,
) -> Result<()> {
    let mut seen_native: HashSet<&str> = HashSet::new();
    let mut seen_linked: HashSet<(&str, &str)> = HashSet::new();
    for (link_id, name) in entries {
        match link_id {
            Some(link_id) => {
                if !allowed_link_ids.contains(link_id) {
                    if let Some(found_in) = depends_list_containing(depends_on, link_id) {
                        return Err(ParsingError::EmitsLinkIdNotImplements {
                            section: section.to_owned(),
                            link_id: link_id.to_owned(),
                            found_in: found_in.to_owned(),
                        }
                        .into());
                    }
                    return Err(ParsingError::UndeclaredEmitsLinkId {
                        section: section.to_owned(),
                        link_id: link_id.to_owned(),
                    }
                    .into());
                }
                if !seen_linked.insert((link_id, name)) {
                    return Err(ParsingError::DuplicateInterfaceEntry {
                        section: section.to_owned(),
                        key: format!("{link_id}:{name}"),
                    }
                    .into());
                }
            }
            None => {
                if !seen_native.insert(name) {
                    return Err(ParsingError::DuplicateInterfaceEntry {
                        section: section.to_owned(),
                        key: name.to_owned(),
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}

/// A pairing slot link_id travels the wire as a keyexpr segment, so it must
/// obey the segment rules from `pmi::wire::Segment` (which this crate cannot
/// name — pmi depends on us): no `/`, no `@`, and not one of the reserved
/// sentinels (`*`, `**`, and the default-link_id sentinel `_`). Emptiness and
/// all-punctuation values are already rejected by the field deserializer.
fn is_wire_safe_link_id(link_id: &str) -> bool {
    !link_id.contains('/')
        && !link_id.contains('@')
        && !matches!(link_id, "*" | "**")
        && link_id != crate::consts::DEFAULT_LINK_ID_SENTINEL
}

/// Validates execution constraints.
fn validate_execution(execution: &Execution) -> Result<()> {
    if let Some(cmds) = &execution.run_cmd
        && cmds.is_empty()
    {
        return Err(ParsingError::EmptyRunCmd.into());
    }

    // `run_cmd` and `container` are mutually exclusive; exactly one must be present.
    match (&execution.run_cmd, &execution.container) {
        (Some(_), Some(_)) => return Err(ParsingError::ProcessAndContainerConflict.into()),
        (None, None) => return Err(ParsingError::NoProcessOrContainer.into()),
        _ => {}
    }

    // Validate container mount paths (reject top-level system directories).
    if let Some(container) = &execution.container
        && let Err((path, blocked_list)) = container.validate()
    {
        return Err(ParsingError::InvalidMountPath(path, blocked_list).into());
    }

    // Validate ${parameters:...} references in mount paths.
    if let Some(container) = &execution.container
        && let Err((ref_path, reason)) = container.validate_parameter_refs(&execution.parameters)
    {
        return Err(ParsingError::InvalidMountPathParameterRef(ref_path, reason).into());
    }

    Ok(())
}

/// Parser responsible for extracting configuration from JSON5 documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<NodeConfig> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        let config: NodeConfig = crate::error::deserialize_json5_with_path(content)?;
        validate_execution(&config.execution)?;
        validate_manifest_links(&config.manifest)?;
        validate_interfaces(&config.manifest, &config.interfaces)?;
        Ok(config)
    }
}

/// Loads a `peppy.json5` at `path` and returns a fully resolved [`NodeConfig`].
///
/// Used by `peppylib`'s standalone mode so Rust and Python nodes can
/// `cargo run` / `python -m` directly from a node directory.
pub fn load_standalone_node_config(path: impl AsRef<Path>) -> Result<NodeConfig> {
    NodeConfigParser::from_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::Error,
        node::{ContainerConfig, EndpointKind, EndpointLabel},
    };
    use tempfile::NamedTempFile;

    /// Test helper: borrows the `ContainerConfig` from a parsed config.
    fn container(config: &NodeConfig) -> &ContainerConfig {
        config
            .execution
            .container
            .as_ref()
            .expect("expected container")
    }

    #[test]
    fn test_parse_minimal_config() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "test_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["./target/release/test_node"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.manifest.tag, "v1");
        assert_eq!(
            config.execution.run_cmd.as_ref().unwrap(),
            &vec!["./target/release/test_node"]
        );
        assert!(config.execution.parameters.is_empty());
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "camera_driver",
                tag: "v21",
            },
            interfaces: {
                topics: {
                    emits: [
                        { name: "/camera/image_raw" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["./target/release/camera_driver"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "camera_driver");
        assert_eq!(config.manifest.tag, "v21");
        assert_eq!(
            config.execution.language,
            crate::node::PeppygenLanguage::Rust
        );
        assert_eq!(
            config.execution.run_cmd.as_ref().unwrap(),
            &vec!["./target/release/camera_driver"]
        );
        assert!(config.interfaces.topics.is_some());
    }

    #[test]
    fn test_empty_file() {
        let tmp = NamedTempFile::new().unwrap();
        // Ensure file is empty
        std::fs::write(tmp.path(), b"").unwrap();
        let result = NodeConfigParser::from_path(tmp.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn test_cannot_read_file() {
        let result = NodeConfigParser::from_path("/path/that/does/not/exist.json5");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(_, _))
        ));
    }

    #[test]
    fn test_cannot_parse_json5() {
        let json5 = r#"{ manifest: [unclosed"#; // invalid JSON5
        let result = NodeConfigParser::from_content(json5);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotParseConfig(_))
        ));
    }

    #[test]
    fn test_parse_container_config() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "container_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(config.execution.run_cmd.is_none());
        assert_eq!(container(&config).def_file, "apptainer.def");
    }

    #[test]
    fn test_process_and_container_conflict() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::ProcessAndContainerConflict)
        ));
    }

    #[test]
    fn test_no_process_or_container() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bare_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::NoProcessOrContainer)
        ));
    }

    #[test]
    fn test_duplicate_link_id_within_nodes_rejected() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "alpha", tag: "v1", link_id: "shared" },
                        { name: "beta",  tag: "v1", link_id: "shared" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId(\"shared\"), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_duplicate_link_id_across_nodes_and_contracts_rejected() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "alpha", tag: "v1", link_id: "shared" },
                    ],
                    contracts: [
                        { name: "depth_camera", tag: "v1", link_id: "shared" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId(\"shared\") across nodes+contracts, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_unique_link_ids_parse_ok() {
        // Same (name, tag) repeated under distinct link_ids must parse.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "openarm01_backbone",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "torso_camera" },
                    ],
                },
            },
            interfaces: {
                topics: {
                    consumes: [
                        { link_id: "wrist_left_camera",  name: "video_stream" },
                        { link_id: "wrist_right_camera", name: "video_stream" },
                        { link_id: "torso_camera",       name: "video_stream" },
                    ],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["cargo", "build", "--release"],
                run_cmd: ["./target/release/backbone"],
            },
        }"#;
        let config =
            NodeConfigParser::from_content(json5).expect("distinct link_ids should parse cleanly");
        let deps = config
            .manifest
            .depends_on
            .expect("depends_on should be set");
        assert_eq!(deps.nodes.len(), 3);
    }

    #[test]
    fn test_dependency_entry_with_from_any_rejected() {
        // `from_any` was removed from the schema: a dependency slot only ever
        // receives from producers the launcher binds to it. A manifest still
        // carrying the flag must fail loudly instead of silently parsing.
        for deps_block in [
            r#"nodes: [{ name: "alpha", tag: "v1", link_id: "a", from_any: true }]"#,
            r#"contracts: [{ name: "beta", tag: "v1", link_id: "b", from_any: true }]"#,
        ] {
            let json5 = format!(
                r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "stale_node",
                    tag: "v1",
                    depends_on: {{ {deps_block} }},
                }},
                execution: {{
                    language: "rust",
                    run_cmd: ["./bin"],
                }},
            }}"#
            );
            let result = NodeConfigParser::from_content(&json5);
            assert!(
                result.is_err(),
                "`from_any` must be rejected as an unknown field in: {deps_block}"
            );
        }
    }

    #[test]
    fn test_pairing_dependency_parses_with_defaults() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "robot_arm",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "controller" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let deps = config.manifest.depends_on.unwrap();
        assert_eq!(deps.pairings.len(), 1);
        let pairing = &deps.pairings[0];
        assert_eq!(pairing.name.as_str(), "arm_link");
        assert_eq!(pairing.role, "arm");
        assert_eq!(pairing.link_id, "controller");
        assert!(pairing.sha256.is_none());
    }

    #[test]
    fn test_same_pairing_twice_under_distinct_link_ids_parses() {
        // The two-arm commander shape: one pairing contract, two slots.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "two_arm_commander",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "controller", link_id: "left_arm" },
                        { name: "arm_link", tag: "v1", role: "controller", link_id: "right_arm" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.depends_on.unwrap().pairings.len(), 2);
    }

    #[test]
    fn observer_slot_consumed_in_topics_parses() {
        // The recorder observes the arm role and taps one of its topics,
        // declared under interfaces.topics.consumes keyed by the slot link_id.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "lerobot_recorder",
                tag: "v1",
                depends_on: {
                    pairing_observers: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm" },
                    ],
                },
            },
            interfaces: {
                topics: {
                    consumes: [
                        { link_id: "observed_arm", name: "joint_states" },
                    ],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        let config = NodeConfigParser::from_content(json5).expect("observer node should parse");
        let deps = config.manifest.depends_on.unwrap();
        let observed = &deps.pairing_observers[0];
        assert_eq!(observed.role, "arm");
        assert_eq!(observed.link_id, "observed_arm");
    }

    #[test]
    fn observer_slot_never_consumed_is_rejected() {
        // An observer that resolves a source but taps no topic is always a
        // mistake, so a slot with no matching consumes entry is rejected.
        // (A participant may legitimately consume nothing.)
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "lerobot_recorder",
                tag: "v1",
                depends_on: {
                    pairing_observers: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm" },
                    ],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        let err = NodeConfigParser::from_content(json5)
            .expect_err("an observer with no consumes entry must be rejected");
        assert!(
            matches!(
                &err,
                Error::Parsing(ParsingError::ObserverLinkNeverConsumed { link_id }) if link_id == "observed_arm"
            ),
            "expected ObserverLinkNeverConsumed(\"observed_arm\"), got: {err:?}"
        );
    }

    #[test]
    fn observer_slot_in_topics_emits_is_rejected() {
        // An observer plays no role and emits nothing, which is knowable from
        // the manifest alone, so a topics.emits entry naming an observer slot
        // is rejected at parse time even when the slot is consumed correctly.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "lerobot_recorder",
                tag: "v1",
                depends_on: {
                    pairing_observers: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm" },
                    ],
                },
            },
            interfaces: {
                topics: {
                    consumes: [
                        { link_id: "observed_arm", name: "joint_states" },
                    ],
                    emits: [
                        { link_id: "observed_arm", name: "joint_states" },
                    ],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        let err = NodeConfigParser::from_content(json5)
            .expect_err("topics.emits naming an observer slot must be rejected");
        assert!(
            matches!(
                &err,
                Error::Parsing(ParsingError::EmitsOnObserverSlot { link_id }) if link_id == "observed_arm"
            ),
            "expected EmitsOnObserverSlot(\"observed_arm\"), got: {err:?}"
        );
    }

    #[test]
    fn test_duplicate_link_id_across_contracts_and_pairings_rejected() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                depends_on: {
                    contracts: [
                        { name: "depth_camera", tag: "v1", link_id: "shared" },
                    ],
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "shared" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId(\"shared\") across contracts+pairings, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_pairing_link_id_with_at_sign_rejected_as_wire_unsafe() {
        // Node/contract link_ids never travel the wire, but a pairing slot
        // link_id is stamped into publish keyexprs, so wire-unsafe characters
        // must be rejected at parse time.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_pairing_node",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "ctrl@home" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::PairingSentinelLinkId(id)) if id == "ctrl@home"
            ),
            "expected PairingSentinelLinkId(\"ctrl@home\"), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn is_wire_safe_link_id_rejects_each_reserved_shape() {
        // Direct coverage of every branch: separator, addressing marker,
        // reserved keyexpr sentinels, default-link_id sentinel.
        for unsafe_id in ["a/b", "ctrl@home", "*", "**", "_"] {
            assert!(
                !is_wire_safe_link_id(unsafe_id),
                "{unsafe_id:?} must be rejected as wire-unsafe"
            );
        }
        assert!(is_wire_safe_link_id("controller"));
    }

    #[test]
    fn test_pairing_link_id_keyexpr_wildcards_rejected() {
        // Like the `_` sentinel below, bare `*` / `**` are caught by the
        // field deserializer's alphanumeric rule before the wire-safety
        // check; the parse must fail one way or another.
        for wildcard in ["*", "**"] {
            let json5 = format!(
                r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "bad_pairing_node",
                    tag: "v1",
                    depends_on: {{
                        pairings: [
                            {{ name: "arm_link", tag: "v1", role: "arm", link_id: "{wildcard}" }},
                        ],
                    }},
                }},
                execution: {{
                    language: "rust",
                    run_cmd: ["./bin"],
                }},
            }}"#
            );
            assert!(
                NodeConfigParser::from_content(&json5).is_err(),
                "pairing link_id {wildcard:?} must fail parse"
            );
        }
    }

    #[test]
    fn test_pairing_link_id_sentinel_rejected() {
        // The bare `_` sentinel never reaches the wire-safety check (the field
        // deserializer's alphanumeric rule rejects it first), but it must fail
        // parse one way or another.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_pairing_node",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "_" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        assert!(NodeConfigParser::from_content(json5).is_err());
    }

    #[test]
    fn test_pairing_entry_with_unknown_field_rejected() {
        // Pairing entries are deny_unknown_fields; a stray key must not
        // silently pass.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_pairing_node",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "controller", extra: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        assert!(NodeConfigParser::from_content(json5).is_err());
    }

    #[test]
    fn test_empty_run_cmd() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "empty_cmd_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: [],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyRunCmd)
        ));
    }

    /// Top-level system directories (e.g. `/tmp`) are blocked as mount sources
    /// because Lima 2.0+ rejects them as guest mount points and binding an
    /// entire system directory into a container is almost always a mistake.
    /// Users should mount a subdirectory instead (e.g. `/tmp/my_app`).
    #[test]
    fn test_container_config_rejects_system_path_mount() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_mount_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/tmp:/tmp:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPath(_, _))
            ),
            "expected InvalidMountPath error, got: {:?}",
            result.unwrap_err()
        );
    }

    /// macOS exposes `/private/tmp`, `/private/var`, etc. as aliases for
    /// `/tmp`, `/var`, etc. The validation strips the `/private` prefix so
    /// these paths are caught by the same blocked-mount-source check.
    #[test]
    fn test_container_config_rejects_private_system_path_mount() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_mount_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/private/tmp:/tmp:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPath(_, _))
            ),
            "expected InvalidMountPath error for /private/tmp, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_config_accepts_subdirectory_mount() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "good_mount_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/tmp/my_app_data:/tmp/my_app_data:rw"],
                },
            },
        }"#;
        let config =
            NodeConfigParser::from_content(json5).expect("subdirectory mount should be accepted");
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["/tmp/my_app_data:/tmp/my_app_data:rw"]
        );
    }

    #[test]
    fn test_container_config_accepts_no_mount_paths() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "no_mount_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).expect("no mount_paths should be valid");
        assert!(container(&config).mount_paths.is_none());
    }

    // NOTE: These parse-time tests assert the raw `${parameters:...}` template
    // strings. Actual substitution with concrete argument values happens at
    // runtime in `resolve_mount_path_parameters()` (core-node-internal), which
    // has its own test coverage.
    #[test]
    fn test_container_mount_path_with_parameter_ref() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "camera_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    device_path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:device_path}:/dev/video0:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("parameter ref in mount path should parse");
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:device_path}:/dev/video0:rw"]
        );
    }

    #[test]
    fn test_container_mount_path_with_nested_parameter_ref() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "camera_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    video: {
                        device_path: "string",
                        frame_rate: "u16",
                    },
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:video.device_path}:/dev/video0:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("nested parameter ref in mount path should parse");
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:video.device_path}:/dev/video0:rw"]
        );
    }

    #[test]
    fn test_container_mount_path_rejects_unknown_parameter_ref() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_ref_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    device_path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:nonexistent}:/data:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPathParameterRef(ref_path, _))
                    if ref_path == "nonexistent"
            ),
            "expected InvalidMountPathParameterRef error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_mount_path_rejects_non_string_parameter_ref() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_type_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    frame_rate: "u16",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:frame_rate}:/data:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPathParameterRef(ref_path, reason))
                    if ref_path == "frame_rate" && reason.contains("string")
            ),
            "expected InvalidMountPathParameterRef error about string type, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_mount_path_skips_blocked_check_for_parameter_ref() {
        // A mount path whose source is a parameter reference should NOT be rejected
        // at parse time, even though the resolved value might be a blocked path.
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dynamic_mount_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:path}:/container/data:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("parameter ref source should skip blocked-path check at parse time");
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:path}:/container/data:rw"]
        );
    }

    #[test]
    fn test_container_config_extra_args_default_to_none() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "container_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(container(&config).apptainer_build_extra_args.is_none());
        assert!(container(&config).apptainer_run_extra_args.is_none());
        assert!(container(&config).lima_shell_extra_args.is_none());
    }

    #[test]
    fn test_container_config_parses_extra_args() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "container_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    apptainer_build_extra_args: ["--no-setgroups", "--force"],
                    apptainer_run_extra_args: ["--no-setgroups"],
                    lima_shell_extra_args: ["--timeout", "30"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let c = container(&config);
        assert_eq!(
            c.apptainer_build_extra_args.as_deref().unwrap(),
            &["--no-setgroups", "--force"]
        );
        assert_eq!(
            c.apptainer_run_extra_args.as_deref().unwrap(),
            &["--no-setgroups"]
        );
        assert_eq!(
            c.lima_shell_extra_args.as_deref().unwrap(),
            &["--timeout", "30"]
        );
    }

    #[test]
    fn test_container_config_parses_empty_extra_args() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "container_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    apptainer_build_extra_args: [],
                    apptainer_run_extra_args: [],
                    lima_shell_extra_args: [],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let c = container(&config);
        assert_eq!(
            c.apptainer_build_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            c.apptainer_run_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            c.lima_shell_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
    }

    /// A manifest with the given `execution.endpoints` body, the rest minimal.
    fn config_with_endpoints(endpoints: &str) -> String {
        format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "panel_node", tag: "v1" }},
                execution: {{
                    language: "rust",
                    run_cmd: ["./bin"],
                    endpoints: {endpoints},
                }},
            }}"#
        )
    }

    #[test]
    fn endpoints_parse_with_one_and_with_several_labels() {
        let one = NodeConfigParser::from_content(&config_with_endpoints(
            r#"{ panel: { kind: "page", description: "The operator panel." } }"#,
        ))
        .expect("one endpoint parses");
        let panel = one
            .execution
            .endpoints
            .get("panel")
            .expect("the declared label");
        assert_eq!(panel.kind, EndpointKind::Page);
        assert_eq!(panel.description, "The operator panel.");

        let several = NodeConfigParser::from_content(&config_with_endpoints(
            r#"{
                viewer: { kind: "page", description: "The viewer page." },
                camera_v1: { kind: "mcp", description: "The camera exposure." },
                panel2: { kind: "page", description: "A second panel." },
            }"#,
        ))
        .expect("several endpoints parse");
        let labels: Vec<&str> = several
            .execution
            .endpoints
            .keys()
            .map(EndpointLabel::as_str)
            .collect();
        assert_eq!(labels, ["camera_v1", "panel2", "viewer"]);
        assert_eq!(
            several.execution.endpoints["camera_v1"].kind,
            EndpointKind::Mcp
        );
    }

    #[test]
    fn endpoints_absent_parse_as_empty() {
        let config = NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "plain_node", tag: "v1" },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        )
        .expect("a manifest without endpoints parses");
        assert!(config.execution.endpoints.is_empty());
    }

    #[test]
    fn an_invalid_endpoint_label_is_refused_naming_it() {
        for label in ["Panel", "1panel", "pa-nel", ""] {
            let json5 = config_with_endpoints(&format!(
                r#"{{ "{label}": {{ kind: "page", description: "A page." }} }}"#
            ));
            let error =
                NodeConfigParser::from_content(&json5).expect_err("the label must be refused");
            match &error {
                Error::Parsing(ParsingError::InvalidEndpointLabel { label: named }) => {
                    assert_eq!(named, label);
                }
                other => panic!("label `{label}`: expected InvalidEndpointLabel, got {other:?}"),
            }
            assert!(
                error.to_string().contains(&format!("`{label}`")),
                "the message names the label: {error}"
            );
        }
    }

    #[test]
    fn both_endpoint_kinds_parse() {
        let config = NodeConfigParser::from_content(&config_with_endpoints(
            r#"{
                page_one: { kind: "page", description: "A page." },
                mcp_one: { kind: "mcp", description: "An MCP endpoint." },
            }"#,
        ))
        .expect("both kinds parse");
        assert_eq!(
            config.execution.endpoints["page_one"].kind,
            EndpointKind::Page
        );
        assert_eq!(
            config.execution.endpoints["mcp_one"].kind,
            EndpointKind::Mcp
        );
    }

    #[test]
    fn a_missing_or_unknown_endpoint_kind_is_refused_naming_the_label_and_the_kinds() {
        let cases = [
            (r#"{ panel: { description: "A page." } }"#, ""),
            (
                r#"{ panel: { kind: "web", description: "A page." } }"#,
                "web",
            ),
            (
                r#"{ panel: { kind: "MCP", description: "A page." } }"#,
                "MCP",
            ),
            (r#"{ panel: { kind: "", description: "A page." } }"#, ""),
        ];
        for (endpoints, expected_kind) in cases {
            let error = NodeConfigParser::from_content(&config_with_endpoints(endpoints))
                .expect_err("the kind must be refused");
            match &error {
                Error::Parsing(ParsingError::InvalidEndpointKind { label, kind }) => {
                    assert_eq!(label, "panel");
                    assert_eq!(kind, expected_kind);
                }
                other => panic!("{endpoints}: expected InvalidEndpointKind, got {other:?}"),
            }
            let message = error.to_string();
            assert!(message.contains("`panel`"), "names the label: {message}");
            assert!(
                message.contains("`page`") && message.contains("`mcp`"),
                "lists both kinds: {message}"
            );
        }
    }

    #[test]
    fn a_missing_or_empty_endpoint_description_is_refused_naming_the_label() {
        for endpoints in [
            r#"{ viewer: { kind: "page" } }"#,
            r#"{ viewer: { kind: "page", description: "" } }"#,
            r#"{ viewer: { kind: "page", description: "   " } }"#,
        ] {
            let error = NodeConfigParser::from_content(&config_with_endpoints(endpoints))
                .expect_err("the description must be refused");
            match &error {
                Error::Parsing(ParsingError::EmptyEndpointDescription { label }) => {
                    assert_eq!(label, "viewer");
                }
                other => panic!("{endpoints}: expected EmptyEndpointDescription, got {other:?}"),
            }
            assert!(
                error.to_string().contains("`viewer`"),
                "names the label: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_key_in_an_endpoint_declaration_is_refused() {
        let error = NodeConfigParser::from_content(&config_with_endpoints(
            r#"{ panel: { kind: "page", description: "A page.", port: 8765 } }"#,
        ))
        .expect_err("an unknown key must be refused");
        assert!(
            error.to_string().contains("port"),
            "the refusal names the stray key: {error}"
        );
    }

    #[test]
    fn execution_round_trips_through_serde_with_endpoints() {
        let config = NodeConfigParser::from_content(&config_with_endpoints(
            r#"{
                panel: { kind: "page", description: "The operator panel." },
                camera_v1: { kind: "mcp", description: "The camera exposure." },
            }"#,
        ))
        .expect("parses");
        let json = serde_json::to_string(&config.execution).expect("serializes");
        assert!(
            json.contains(r#""endpoints":{"camera_v1":{"kind":"mcp","description":"The camera exposure."},"panel":{"kind":"page","description":"The operator panel."}}"#),
            "{json}"
        );
        let restored: Execution = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(restored.endpoints, config.execution.endpoints);

        let plain: Execution = serde_json::from_str(r#"{"language":"rust","run_cmd":["./bin"]}"#)
            .expect("deserializes");
        assert!(plain.endpoints.is_empty());
        assert!(
            !serde_json::to_string(&plain)
                .expect("serializes")
                .contains("endpoints"),
            "an empty declaration set stays off the wire"
        );
    }

    #[test]
    fn test_parse_config_execution_without_language_rejected() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "test_node",
                tag: "v1",
            },
            execution: {
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::MissingExecutionLanguage)
        ));
    }

    #[test]
    fn test_node_error_message_includes_field_path() {
        // run_cmd should be an array, not a map
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "test_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: { wrong: "type" },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        let Error::Parsing(ParsingError::CannotParseConfig(msg)) = result.unwrap_err() else {
            panic!("expected CannotParseConfig error");
        };
        assert!(
            msg.contains("execution.run_cmd"),
            "error should include field path, got: {msg}"
        );
    }

    #[test]
    fn load_standalone_returns_resolved_config() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();

        let node_config =
            load_standalone_node_config(tmp.path()).expect("config should load cleanly");
        assert_eq!(node_config.manifest.name.as_str(), "my_node");
        assert_eq!(
            node_config.execution.run_cmd.as_deref(),
            Some(["./target/debug/my_node".to_string()].as_slice())
        );
    }

    /// Future-proofing test: when a new field is added to [`NodeConfig`], this
    /// exhaustive destructuring will fail to compile, forcing the author to
    /// think about how it should be handled.
    #[test]
    fn node_config_field_set_is_complete() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();

        let merged = load_standalone_node_config(tmp.path()).unwrap();

        // Exhaustive destructuring — add-a-field will fail compilation here.
        let NodeConfig {
            peppy_schema,
            manifest,
            interfaces,
            execution,
        } = merged;

        assert_eq!(peppy_schema, crate::schema::PeppySchema::NodeV1);
        assert_eq!(manifest.name.as_str(), "my_node");
        assert_eq!(manifest.tag, "v1");
        assert_eq!(interfaces, crate::node::Interfaces::default());
        assert_eq!(
            execution.run_cmd.as_deref(),
            Some(["./target/debug/my_node".to_string()].as_slice())
        );
    }

    // ─── manifest.implements + produced-entry Tier A validation ─────────────

    /// Wraps an implements list + interfaces block in a minimal node config.
    fn node_with(implements: &str, interfaces: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "camera_node",
                tag: "v1",
                implements: [{implements}],
            }},
            interfaces: {interfaces},
            execution: {{
                language: "rust",
                run_cmd: ["./bin"],
            }},
        }}"#
        )
    }

    #[test]
    fn test_implements_with_full_coverage_entries_parses() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{
                topics: { emits: [{ link_id: "cam", name: "video_stream" }] },
                services: { exposes: [{ link_id: "cam", name: "video_stream_info" }] },
            }"#,
        );
        let config = NodeConfigParser::from_content(&json5).expect("should parse");
        assert_eq!(config.manifest.implements.len(), 1);
        assert_eq!(config.manifest.implements[0].link_id, "cam");
    }

    #[test]
    fn test_old_conforms_to_manifest_rejected_as_unknown_field() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "old_node", tag: "v1" },
            interfaces: {
                conforms_to: [{ name: "uvc_camera", tag: "v1" }],
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        let Error::Parsing(ParsingError::CannotParseConfig(msg)) = result.unwrap_err() else {
            panic!("expected plain serde unknown-field rejection");
        };
        assert!(
            msg.contains("conforms_to"),
            "error should name the unknown field, got: {msg}"
        );
    }

    #[test]
    fn test_duplicate_implements_contract_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam_a" },
               { name: "uvc_camera", tag: "v1", link_id: "cam_b" }"#,
            r#"{}"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateImplementsContract { name, tag })
                    if name == "uvc_camera" && tag == "v1"
            ),
            "expected DuplicateImplementsContract, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_implements_sanitized_tag_collision_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v-1", link_id: "cam_a" },
               { name: "uvc_camera", tag: "v_1", link_id: "cam_b" }"#,
            r#"{}"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::ImplementsTagSanitizationCollision { name, .. })
                    if name == "uvc_camera"
            ),
            "expected ImplementsTagSanitizationCollision, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_implements_link_id_collides_with_depends_on_link_id() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                implements: [{ name: "uvc_camera", tag: "v1", link_id: "shared" }],
                depends_on: {
                    nodes: [{ name: "alpha", tag: "v1", link_id: "shared" }],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId across implements+depends_on, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_emits_link_id_naming_depends_on_slot_rejected() {
        for (deps_block, found_in) in [
            (
                r#"nodes: [{ name: "alpha", tag: "v1", link_id: "dep" }]"#,
                "nodes",
            ),
            (
                r#"contracts: [{ name: "uvc_camera", tag: "v1", link_id: "dep" }]"#,
                "contracts",
            ),
        ] {
            let json5 = format!(
                r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "wrong_direction",
                    tag: "v1",
                    depends_on: {{ {deps_block} }},
                }},
                interfaces: {{
                    topics: {{ emits: [{{ link_id: "dep", name: "video_stream" }}] }},
                }},
                execution: {{ language: "rust", run_cmd: ["./bin"] }},
            }}"#
            );
            let result = NodeConfigParser::from_content(&json5);
            assert!(
                matches!(
                    result.as_ref().unwrap_err(),
                    Error::Parsing(ParsingError::EmitsLinkIdNotImplements { link_id, found_in: f, .. })
                        if link_id == "dep" && f == found_in
                ),
                "expected EmitsLinkIdNotImplements({found_in}), got: {:?}",
                result.unwrap_err()
            );
        }
    }

    /// Wraps a pairing slot + interfaces block in a minimal node config. The
    /// slot is always `arm_link/v1` under link_id `arm`.
    fn node_with_pairing(interfaces: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "arm_controller",
                tag: "v1",
                depends_on: {{
                    pairings: [{{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }}],
                }},
            }},
            interfaces: {interfaces},
            execution: {{ language: "rust", run_cmd: ["./bin"] }},
        }}"#
        )
    }

    #[test]
    fn test_pairing_link_id_in_topics_emits_parses() {
        let json5 = node_with_pairing(
            r#"{ topics: { emits: [{ link_id: "arm", name: "joint_commands" }] } }"#,
        );
        let config = NodeConfigParser::from_content(&json5).expect("pairing-backed emit parses");
        let emits = config.interfaces.topics.unwrap().emits.unwrap();
        assert_eq!(
            emits[0].link_id(),
            Some("arm"),
            "a pairing slot is a legal emits target"
        );
        assert_eq!(emits[0].name(), "joint_commands");
    }

    #[test]
    fn test_pairing_link_id_in_topics_consumes_parses() {
        let json5 = node_with_pairing(
            r#"{ topics: { consumes: [{ link_id: "arm", name: "joint_states" }] } }"#,
        );
        let config = NodeConfigParser::from_content(&json5).expect("pairing-backed consume parses");
        let consumes = config.interfaces.topics.unwrap().consumes.unwrap();
        assert_eq!(consumes[0].link_id, "arm");
        assert_eq!(consumes[0].name, "joint_states");
    }

    /// A pairing document declares topics only, so its slot may not be named
    /// by a service or action entry in either direction.
    #[test]
    fn test_pairing_link_id_in_service_and_action_exposes_rejected() {
        for (section, interfaces) in [
            (
                "services.exposes",
                r#"{ services: { exposes: [{ link_id: "arm", name: "calibrate" }] } }"#,
            ),
            (
                "actions.exposes",
                r#"{ actions: { exposes: [{ link_id: "arm", name: "home" }] } }"#,
            ),
        ] {
            let result = NodeConfigParser::from_content(&node_with_pairing(interfaces));
            assert!(
                matches!(
                    result.as_ref().unwrap_err(),
                    Error::Parsing(ParsingError::EmitsLinkIdNotImplements { section: s, link_id, found_in })
                        if s == section && link_id == "arm" && found_in == "pairings"
                ),
                "expected EmitsLinkIdNotImplements({section}) naming depends_on.pairings, got: {:?}",
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn test_pairing_backed_entry_with_inline_shape_rejected() {
        for (section, field, interfaces) in [
            (
                "topics.emits",
                "qos_profile",
                r#"{ topics: { emits: [{ link_id: "arm", name: "joint_commands", qos_profile: "reliable" }] } }"#,
            ),
            (
                "topics.emits",
                "message_format",
                r#"{ topics: { emits: [{ link_id: "arm", name: "joint_commands", message_format: { fields: { x: "f64" } } }] } }"#,
            ),
        ] {
            let result = NodeConfigParser::from_content(&node_with_pairing(interfaces));
            assert!(
                matches!(
                    result.as_ref().unwrap_err(),
                    Error::Parsing(ParsingError::LinkedEntryWithInlineShape { section: s, field: f, link_id, .. })
                        if s == section && f == field && link_id == "arm"
                ),
                "expected LinkedEntryWithInlineShape({field}), got: {:?}",
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn test_duplicate_pairing_backed_emit_rejected() {
        let json5 = node_with_pairing(
            r#"{ topics: { emits: [
                { link_id: "arm", name: "joint_commands" },
                { link_id: "arm", name: "joint_commands" },
            ] } }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateInterfaceEntry { key, .. })
                    if key == "arm:joint_commands"
            ),
            "expected DuplicateInterfaceEntry, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_emits_link_id_matching_nothing_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{ topics: { emits: [{ link_id: "typo", name: "video_stream" }] } }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::UndeclaredEmitsLinkId { link_id, .. })
                    if link_id == "typo"
            ),
            "expected UndeclaredEmitsLinkId, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_consumed_item_referencing_implements_link_id_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{
                topics: {
                    emits: [{ link_id: "cam", name: "video_stream" }],
                    consumes: [{ link_id: "cam", name: "video_stream" }],
                },
            }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::ConsumedItemReferencesImplementsLinkId { link_id })
                    if link_id == "cam"
            ),
            "expected ConsumedItemReferencesImplementsLinkId, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_duplicate_native_emit_name_rejected() {
        let json5 = node_with(
            "",
            r#"{
                topics: { emits: [
                    { name: "stream", message_format: { x: "f64" } },
                    { name: "stream", message_format: { y: "f64" } },
                ] },
            }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateInterfaceEntry { key, .. })
                    if key == "stream"
            ),
            "expected DuplicateInterfaceEntry, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_duplicate_contract_backed_entry_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{
                topics: { emits: [
                    { link_id: "cam", name: "video_stream" },
                    { link_id: "cam", name: "video_stream" },
                ] },
            }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateInterfaceEntry { key, .. })
                    if key == "cam:video_stream"
            ),
            "expected DuplicateInterfaceEntry, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_native_and_contract_backed_same_name_coexist() {
        // Allowed on the producer: the two are namespaced apart in modules,
        // schema keys, and wire keys, and node-dep consumers resolve only the
        // native one (contract-backed interfaces are consumed via
        // depends_on.contracts).
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{
                topics: { emits: [
                    { link_id: "cam", name: "video_stream" },
                    { name: "video_stream", message_format: { x: "f64" } },
                ] },
            }"#,
        );
        NodeConfigParser::from_content(&json5)
            .expect("native + contract-backed same-name coexistence must parse");
    }

    #[test]
    fn test_contract_backed_entry_with_inline_shape_rejected() {
        let json5 = node_with(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#,
            r#"{
                topics: { emits: [
                    { link_id: "cam", name: "video_stream", qos_profile: "sensor_data" },
                ] },
            }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::LinkedEntryWithInlineShape { field, .. })
                    if field == "qos_profile"
            ),
            "expected LinkedEntryWithInlineShape, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_empty_interface_name_rejected_via_structured_error() {
        let json5 = node_with(
            "",
            r#"{ topics: { emits: [{ message_format: { x: "f64" } }] } }"#,
        );
        let result = NodeConfigParser::from_content(&json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::EmptyInterfaceName { section })
                    if section == "topics.emits"
            ),
            "expected EmptyInterfaceName, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_relay_shape_parses() {
        // Implementing AND depending on the same contract (name, tag) under
        // distinct link_ids is legal (relay shape).
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "relay",
                tag: "v1",
                implements: [{ name: "uvc_camera", tag: "v1", link_id: "cam_out" }],
                depends_on: {
                    contracts: [{ name: "uvc_camera", tag: "v1", link_id: "cam_in" }],
                },
            },
            interfaces: {
                topics: {
                    emits: [{ link_id: "cam_out", name: "video_stream" }],
                    consumes: [{ link_id: "cam_in", name: "video_stream" }],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#;
        NodeConfigParser::from_content(json5).expect("relay shape must parse");
    }

    // ─── `refine` on document-backed entries ────────────────────────────────

    /// A node that implements a contract, consumes another, and plays a
    /// pairing role, with a `refine` block on every document-backed entry
    /// kind in both directions. `{refine}` is spliced in as the block's
    /// body where the test needs a variation.
    fn node_with_refinements() -> String {
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "seven_dof_arm",
                tag: "v1",
                implements: [{ name: "limb_motion", tag: "v1", link_id: "moves" }],
                depends_on: {
                    contracts: [{ name: "limb_motion", tag: "v1", link_id: "upstream" }],
                    pairings: [{ name: "joint_link", tag: "v1", role: "follower", link_id: "leader" }],
                },
            },
            interfaces: {
                topics: {
                    emits: [
                        { link_id: "moves", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } },
                        { link_id: "leader", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } },
                    ],
                    consumes: [
                        { link_id: "upstream", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } },
                        { link_id: "leader", name: "joint_setpoints", refine: { message_format: { positions: { $length: 7 } } } },
                    ],
                },
                services: {
                    exposes: [
                        { link_id: "moves", name: "set_joints", refine: { request_message_format: { targets: { $length: 7 } } } },
                    ],
                    consumes: [
                        { link_id: "upstream", name: "set_joints", refine: { response_message_format: { measured: { $length: 7 } } } },
                    ],
                },
                actions: {
                    exposes: [
                        {
                            link_id: "moves",
                            name: "move_arm_joints",
                            refine: {
                                goal_service: { request_message_format: { joint_positions: { $length: 7 } } },
                                result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },
                            },
                        },
                    ],
                    consumes: [
                        { link_id: "upstream", name: "move_arm_joints", refine: { feedback_topic: { message_format: { progress: { $length: 7 } } } } },
                    ],
                },
            },
            execution: { language: "rust", run_cmd: ["./bin"] },
        }"#
        .to_string()
    }

    #[test]
    fn refine_parses_on_every_document_backed_entry_kind() {
        let config = NodeConfigParser::from_content(&node_with_refinements())
            .expect("refinements on document-backed entries parse");
        let topics = config.interfaces.topics.as_ref().unwrap();
        for emitted in topics.emits.as_ref().unwrap() {
            let crate::node::EmittedTopic::Linked(linked) = emitted else {
                panic!("every emit here is document-backed");
            };
            let refine = linked.refine.as_ref().expect("emit carries its refinement");
            assert_eq!(
                refine.message_format.0["positions"],
                crate::node::FieldRefinement::Length(7)
            );
        }
        for consumed in topics.consumes.as_ref().unwrap() {
            assert!(
                consumed.refine.is_some(),
                "consume `{}` keeps its refinement",
                consumed.name
            );
        }
        let services = config.interfaces.services.as_ref().unwrap();
        let crate::node::ExposedService::Linked(exposed) = &services.exposes.as_ref().unwrap()[0]
        else {
            panic!("the exposed service is document-backed");
        };
        assert!(
            exposed
                .refine
                .as_ref()
                .unwrap()
                .request_message_format
                .is_some()
        );
        assert!(
            services.consumes.as_ref().unwrap()[0]
                .refine
                .as_ref()
                .unwrap()
                .response_message_format
                .is_some()
        );
        let actions = config.interfaces.actions.as_ref().unwrap();
        let crate::node::ExposedAction::Linked(exposed) = &actions.exposes.as_ref().unwrap()[0]
        else {
            panic!("the exposed action is document-backed");
        };
        let refine = exposed.refine.as_ref().unwrap();
        assert!(refine.goal_service.is_some() && refine.result_service.is_some());
        assert!(
            actions.consumes.as_ref().unwrap()[0]
                .refine
                .as_ref()
                .unwrap()
                .feedback_topic
                .is_some()
        );
    }

    /// Serialization keeps the refinements, and normalization sorts their
    /// keys, so two manifests that pin the same arrays in a different order
    /// compare equal.
    #[test]
    fn refine_round_trips_and_normalizes() {
        let config = NodeConfigParser::from_content(&node_with_refinements()).unwrap();
        let serialized = serde_json5::to_string(&config).expect("serializes");
        let reparsed: NodeConfig = serde_json5::from_str(&serialized).expect("re-parses");
        assert_eq!(config.interfaces, reparsed.interfaces);

        let reordered = node_with_refinements().replace(
            "goal_service: { request_message_format: { joint_positions: { $length: 7 } } },\n                                result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },",
            "result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },\n                                goal_service: { request_message_format: { joint_positions: { $length: 7 } } },",
        );
        assert_ne!(
            reordered,
            node_with_refinements(),
            "the replacement must apply"
        );
        let reordered = NodeConfigParser::from_content(&reordered).unwrap();
        assert!(config.interfaces.matches_unordered(&reordered.interfaces));
    }

    #[test]
    fn refine_on_native_entry_rejected_per_section() {
        for (section, interfaces) in [
            (
                "topics.emits",
                r#"{ topics: { emits: [{ name: "joint_states", message_format: { positions: { $type: "array", $items: "f64" } }, refine: { message_format: { positions: { $length: 7 } } } }] } }"#,
            ),
            (
                "services.exposes",
                r#"{ services: { exposes: [{ name: "set_joints", refine: { request_message_format: { targets: { $length: 7 } } } }] } }"#,
            ),
            (
                "actions.exposes",
                r#"{ actions: { exposes: [{ name: "move", refine: { goal_service: { request_message_format: { targets: { $length: 7 } } } } }] } }"#,
            ),
        ] {
            let result = NodeConfigParser::from_content(&node_with("", interfaces));
            assert!(
                matches!(
                    result.as_ref().unwrap_err(),
                    Error::Parsing(ParsingError::RefineWithoutLinkId { section: s, .. }) if s == section
                ),
                "{section}: expected RefineWithoutLinkId, got: {:?}",
                result.unwrap_err()
            );
        }
    }

    /// A `depends_on.nodes` slot resolves to a producer's native shape, which
    /// the consumer takes as is: there is no document whose generic arrays a
    /// refinement could pin.
    #[test]
    fn refine_on_node_slot_consume_rejected_per_section() {
        for (section, interfaces) in [
            (
                "topics.consumes",
                r#"{ topics: { consumes: [{ link_id: "cam", name: "video_stream", refine: { message_format: { frame: { $length: 4 } } } }] } }"#,
            ),
            (
                "services.consumes",
                r#"{ services: { consumes: [{ link_id: "cam", name: "info", refine: { response_message_format: { size: { $length: 2 } } } }] } }"#,
            ),
            (
                "actions.consumes",
                r#"{ actions: { consumes: [{ link_id: "cam", name: "focus", refine: { goal_service: { request_message_format: { at: { $length: 2 } } } } }] } }"#,
            ),
        ] {
            let json5 = format!(
                r#"{{
                    peppy_schema: "node/v1",
                    manifest: {{
                        name: "viewer",
                        tag: "v1",
                        depends_on: {{ nodes: [{{ name: "uvc_camera", tag: "v1", link_id: "cam" }}] }},
                    }},
                    interfaces: {interfaces},
                    execution: {{ language: "rust", run_cmd: ["./bin"] }},
                }}"#
            );
            let result = NodeConfigParser::from_content(&json5);
            assert!(
                matches!(
                    result.as_ref().unwrap_err(),
                    Error::Parsing(ParsingError::RefineOnNodeSlot { section: s, link_id, .. })
                        if s == section && link_id == "cam"
                ),
                "{section}: expected RefineOnNodeSlot, got: {:?}",
                result.unwrap_err()
            );
        }
    }

    /// The block's own structure is checked at parse time: an empty block,
    /// a shape modifier inside it, or a leaf mixing `$length` with nested
    /// fields is rejected before any document resolves.
    #[test]
    fn malformed_refine_blocks_rejected_at_parse_time() {
        for (interfaces, needle) in [
            (
                r#"{ topics: { emits: [{ link_id: "moves", name: "joint_states", refine: {} }] } }"#,
                "message_format",
            ),
            (
                r#"{ topics: { emits: [{ link_id: "moves", name: "joint_states", refine: { message_format: {} } }] } }"#,
                "empty format refinement",
            ),
            (
                r#"{ topics: { emits: [{ link_id: "moves", name: "joint_states", refine: { message_format: { positions: { $type: "array", $length: 7 } } } }] } }"#,
                "unknown modifier `$type`",
            ),
            (
                r#"{ services: { exposes: [{ link_id: "moves", name: "set_joints", refine: {} }] } }"#,
                "request_message_format",
            ),
            (
                r#"{ actions: { exposes: [{ link_id: "moves", name: "move", refine: { goal_service: {} } }] } }"#,
                "request_message_format",
            ),
        ] {
            let err = NodeConfigParser::from_content(&node_with(
                r#"{ name: "limb_motion", tag: "v1", link_id: "moves" }"#,
                interfaces,
            ))
            .expect_err("malformed refine blocks must be rejected")
            .to_string();
            assert!(err.contains(needle), "expected `{needle}` in: {err}");
        }
    }
}
