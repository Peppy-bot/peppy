//! Pairing-document resolution for `depends_on.pairings`: loading the
//! `pairing/v1` doc from the repo cache (sha-pinned, drift-checked) and
//! validating each manifest entry's role against the doc's two roles.

use crate::services::repo::cache as repo_cache;
use config::PairingCoverageMismatch;
use daemon_config::consts::PeppyDirs;
use daemon_config::pairing::PeppyPairing;
use std::collections::HashMap;

/// Loads a `PeppyPairing` document from the local pairing cache for
/// `(name, tag)`, verifying both the SHA pin (when set) and on-disk drift
/// against the cached fingerprint. Production goes through
/// [`resolve_pairing_doc_cached`] via [`validate_pairing_specs`]; this
/// load-then-resolve wrapper only backs the sha-pin/drift tests below.
#[cfg(test)]
fn resolve_pairing_doc(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PeppyPairing, String> {
    let cache = repo_cache::load_pairing_cache(peppy_dirs)
        .map_err(|e| format!("failed to load pairing cache: {e}"))?;
    resolve_pairing_doc_cached(&cache, peppy_dirs, name, tag, sha256_pin, on_feedback)
}

/// Resolves one pairing document against an already-loaded cache, so
/// multi-slot validation doesn't re-read `pairings.json5` per entry.
fn resolve_pairing_doc_cached(
    cache: &[repo_cache::PairingCacheEntry],
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PeppyPairing, String> {
    let entry = match sha256_pin {
        Some(sha) => repo_cache::lookup_pairing_by_sha256(cache, name, tag, sha),
        None => repo_cache::lookup_pairing(cache, name, tag),
    };

    repo_cache::resolve_cached_doc(
        peppy_dirs,
        "pairing",
        &format!("{name}:{tag}"),
        sha256_pin,
        entry.map(Into::into),
        |content| {
            daemon_config::pairing::PeppyPairingParser::from_content(content)
                .map_err(|e| e.to_string())
        },
        on_feedback,
    )
}

/// Validates every `depends_on.pairings` entry of a manifest against its
/// resolved pairing document and returns the resolved docs keyed by slot
/// link_id (ready for the codegen collection step):
///
/// - the declared `role` must be one of the doc's two roles (the error
///   names them).
///
/// No tag-collision rule applies here: pairing slots are addressed by
/// `link_id` everywhere (module paths, wire segments), and link_id
/// uniqueness is enforced at parse time. The tag identifies the resolved
/// pairing artifact instead: with the name and sha256 pin it forms the
/// `(name, tag, sha256)` key each slot's document is looked up and cached
/// under.
pub(crate) fn validate_pairing_specs(
    manifest: &config::node::Manifest,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<HashMap<String, PeppyPairing>, String> {
    let Some(pairing_deps) = manifest
        .depends_on
        .as_ref()
        .map(|d| d.pairings.as_slice())
        .filter(|p| !p.is_empty())
    else {
        return Ok(HashMap::new());
    };

    let cache = repo_cache::load_pairing_cache(peppy_dirs)
        .map_err(|e| format!("failed to load pairing cache: {e}"))?;

    // Two slots referencing the same document (e.g. a commander driving two
    // arms over the same pairing) resolve it once, not once per slot.
    let mut resolved: HashMap<(&str, &str, Option<&str>), PeppyPairing> = HashMap::new();
    let mut out = HashMap::new();
    for dep in pairing_deps {
        let name = dep.name.as_str();
        let doc = match resolved.entry((name, dep.tag.as_str(), dep.sha256.as_deref())) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => v.insert(resolve_pairing_doc_cached(
                &cache,
                peppy_dirs,
                name,
                &dep.tag,
                dep.sha256.as_deref(),
                on_feedback,
            )?),
        };
        if !doc.has_role(&dep.role) {
            let declared: Vec<&str> = doc.roles.iter().map(|r| r.as_str()).collect();
            return Err(format!(
                "pairing slot `{}`: role `{}` is not declared by pairing `{name}:{}` \
                 (declared roles: [{}])",
                dep.link_id,
                dep.role,
                dep.tag,
                declared.join(", "),
            ));
        }
        out.insert(dep.link_id.clone(), doc.clone());
    }
    Ok(out)
}

/// A failure resolving `depends_on.pairings` against the declared interface
/// entries. Mirrors `ImplementsError`: coverage problems are aggregated per
/// slot so a node with several wrong entries produces one readable report.
#[derive(Debug)]
pub enum PairingError {
    /// One aggregated set-diff per broken slot.
    Coverage(Vec<PairingCoverageMismatch>),
    Other(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coverage(mismatches) => {
                for (idx, mismatch) in mismatches.iter().enumerate() {
                    if idx > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{mismatch}")?;
                }
                Ok(())
            }
            Self::Other(reason) => write!(f, "{reason}"),
        }
    }
}

impl From<String> for PairingError {
    fn from(reason: String) -> Self {
        Self::Other(reason)
    }
}

/// Per-slot bookkeeping while walking the node's pairing-backed entries.
#[derive(Default)]
struct SlotCoverage {
    /// How many `topics.emits` entries resolved to each of the role's topics.
    visited_emits: HashMap<String, u32>,
    /// How many `topics.consumes` entries resolved to each counterpart topic.
    visited_consumes: HashMap<String, u32>,
    unknown_emits: Vec<String>,
    wrong_role_emits: Vec<String>,
    unknown_consumes: Vec<String>,
    wrong_role_consumes: Vec<String>,
}

/// Resolves every `depends_on.pairings` slot into the generator inputs for the
/// `pairings/<link_id>/<topic>` modules, driven by what the node declares:
/// each pairing-backed `topics.emits` entry becomes peer-emitted and each
/// pairing-backed `topics.consumes` entry becomes peer-consumed, with shape
/// and QoS read from the pairing document.
///
/// Coverage mirrors contracts. The emit side must be exact: the entries for a
/// slot are precisely the document's topics whose `emitted_by` is the slot's
/// role, because a role that silently stops emitting one of its topics breaks
/// its peer. The consume side may be partial: omitting a topic means no module
/// is generated and no subscription can be created for it, which is a local
/// decision with no effect on the peer.
///
/// Runs [`validate_pairing_specs`] first, so role errors and cache misses
/// surface before any of this.
pub fn collect_pairing_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<generator::DeploymentInterface>, PairingError> {
    let Some(pairing_deps) = manifest
        .depends_on
        .as_ref()
        .map(|d| d.pairings.as_slice())
        .filter(|p| !p.is_empty())
    else {
        return Ok(Vec::new());
    };
    let docs = validate_pairing_specs(manifest, peppy_dirs, on_feedback)?;

    let mut out = Vec::new();
    let mut broken = Vec::new();
    for dep in pairing_deps {
        let doc = docs
            .get(&dep.link_id)
            .expect("validate_pairing_specs returns a doc per declared slot");
        let peer = generator::PeerContext {
            link_id: dep.link_id.clone(),
            pairing_name: dep.name.as_str().to_string(),
            pairing_tag: dep.tag.clone(),
        };
        let mut coverage = SlotCoverage::default();

        for name in declared_topic_names(interfaces_cfg, &dep.link_id, Direction::Emits) {
            let Some(topic) = doc.topics.iter().find(|t| t.name == name) else {
                coverage.unknown_emits.push(name.to_string());
                continue;
            };
            if topic.emitted_by != dep.role {
                coverage
                    .wrong_role_emits
                    .push(format!("{name} (emitted by {})", topic.emitted_by));
                continue;
            }
            *coverage.visited_emits.entry(name.to_string()).or_insert(0) += 1;
            out.push(generator::DeploymentInterface::peer_emitted_topic(
                native_topic(topic),
                peer.clone(),
            ));
        }

        for name in declared_topic_names(interfaces_cfg, &dep.link_id, Direction::Consumes) {
            let Some(topic) = doc.topics.iter().find(|t| t.name == name) else {
                coverage.unknown_consumes.push(name.to_string());
                continue;
            };
            if topic.emitted_by == dep.role {
                coverage
                    .wrong_role_consumes
                    .push(format!("{name} (emitted by this node's role {})", dep.role));
                continue;
            }
            *coverage
                .visited_consumes
                .entry(name.to_string())
                .or_insert(0) += 1;
            out.push(generator::DeploymentInterface::peer_consumed_topic(
                native_topic(topic),
                peer.clone(),
            ));
        }

        let mismatch = build_mismatch(dep, doc, coverage);
        if !mismatch.is_empty() {
            broken.push(mismatch);
        }
    }

    if !broken.is_empty() {
        return Err(PairingError::Coverage(broken));
    }
    Ok(out)
}

/// Which direction of `interfaces.topics` an entry walk is reading.
#[derive(Clone, Copy)]
enum Direction {
    Emits,
    Consumes,
}

/// The names declared for one pairing slot in one direction, in manifest
/// order. Entries naming other slots (contract-backed emits, node-backed
/// consumes) belong to the implements and consumed collectors and are skipped
/// here, which is what keeps pairing topics out of `emitted_topics` and
/// `consumed_topics`.
fn declared_topic_names<'a>(
    interfaces_cfg: &'a config::node::Interfaces,
    link_id: &'a str,
    direction: Direction,
) -> Vec<&'a str> {
    let Some(topics) = interfaces_cfg.topics.as_ref() else {
        return Vec::new();
    };
    match direction {
        Direction::Emits => topics
            .emits
            .iter()
            .flatten()
            .filter(|e| e.link_id() == Some(link_id))
            .map(|e| e.name())
            .collect(),
        Direction::Consumes => topics
            .consumes
            .iter()
            .flatten()
            .filter(|c| c.link_id == link_id)
            .map(|c| c.name.as_str())
            .collect(),
    }
}

/// The generator carries a pairing topic's shape in a `NativeEmittedTopic` in
/// both directions; the document is the sole source of that shape.
fn native_topic(topic: &daemon_config::pairing::PairingTopic) -> config::node::NativeEmittedTopic {
    config::node::NativeEmittedTopic {
        name: topic.name.clone(),
        qos_profile: topic.qos_profile.clone(),
        message_format: topic.message_format.clone(),
    }
}

/// Turns one slot's bookkeeping into its aggregated diff. Only the emit side
/// contributes `missing`/`duplicated` against the document: consume coverage
/// is free, so an unlisted counterpart topic is not a defect, but naming one
/// twice still is.
fn build_mismatch(
    dep: &config::node::PairingDependency,
    doc: &PeppyPairing,
    coverage: SlotCoverage,
) -> PairingCoverageMismatch {
    let mut missing_emits = Vec::new();
    let mut duplicated_emits = Vec::new();
    for topic in doc.topics.iter().filter(|t| t.emitted_by == dep.role) {
        match coverage
            .visited_emits
            .get(&topic.name)
            .copied()
            .unwrap_or(0)
        {
            0 => missing_emits.push(topic.name.clone()),
            1 => {}
            _ => duplicated_emits.push(topic.name.clone()),
        }
    }
    let mut duplicated_consumes: Vec<String> = coverage
        .visited_consumes
        .iter()
        .filter(|(_, visits)| **visits > 1)
        .map(|(name, _)| name.clone())
        .collect();
    duplicated_consumes.sort();

    PairingCoverageMismatch {
        pairing_name: dep.name.as_str().to_string(),
        pairing_tag: dep.tag.clone(),
        link_id: dep.link_id.clone(),
        role: dep.role.clone(),
        missing_emits,
        unknown_emits: coverage.unknown_emits,
        duplicated_emits,
        wrong_role_emits: coverage.wrong_role_emits,
        unknown_consumes: coverage.unknown_consumes,
        duplicated_consumes,
        wrong_role_consumes: coverage.wrong_role_consumes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::RepoSourceKind;
    use std::fs;
    use tempfile::TempDir;

    const ARM_LINK_BODY: &str = r#"{
        peppy_schema: "pairing/v1",
        manifest: { name: "arm_link", tag: "v1" },
        roles: ["controller", "arm"],
        topics: [
            { emitted_by: "controller", name: "joint_commands" },
            { emitted_by: "arm", name: "joint_states" }
        ]
    }"#;

    fn seed_pairing(
        dir: &std::path::Path,
        name: &str,
        tag: &str,
        body: &str,
    ) -> repo_cache::PairingCacheEntry {
        let path = dir.join(format!("{name}_{tag}.json5"));
        fs::write(&path, body).expect("write pairing file");
        repo_cache::PairingCacheEntry {
            pairing_name: name.to_string(),
            tag: tag.to_string(),
            sha256: config::fingerprint::fingerprint_for_bytes(body.as_bytes()),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            path: path.to_string_lossy().to_string(),
            repo_id: 0,
        }
    }

    fn make_peppy_dirs_with_cache(
        entries: &[repo_cache::PairingCacheEntry],
    ) -> (TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let dirs = PeppyDirs::new(tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");
        fs::write(
            repo_cache::pairings_repo_cache_path(&dirs),
            serde_json5::to_string(&entries.to_vec()).expect("serialize cache"),
        )
        .expect("write cache file");
        (tmp, dirs)
    }

    fn manifest(json5: &str) -> config::node::Manifest {
        serde_json5::from_str(json5).expect("manifest parses")
    }

    fn interfaces(json5: &str) -> config::node::Interfaces {
        serde_json5::from_str(json5).expect("interfaces parse")
    }

    /// A node playing the `arm` role of `arm_link/v1` on slot `controller`,
    /// so it emits `joint_states` and may consume `joint_commands`.
    fn arm_manifest() -> config::node::Manifest {
        manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]
                }
            }"#,
        )
    }

    fn seeded_dirs() -> (TempDir, TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (cache_tmp, dirs) = make_peppy_dirs_with_cache(&[entry]);
        (tmp, cache_tmp, dirs)
    }

    fn collect(
        manifest: &config::node::Manifest,
        interfaces_cfg: &config::node::Interfaces,
        dirs: &PeppyDirs,
    ) -> std::result::Result<Vec<generator::DeploymentInterface>, PairingError> {
        collect_pairing_interfaces(manifest, interfaces_cfg, dirs, &|_| {})
    }

    /// The `(module_path, is_emitted)` of each produced interface, so tests
    /// assert both the direction and the `pairings/<link_id>/<topic>` path.
    fn peer_modules(out: &[generator::DeploymentInterface]) -> Vec<(Vec<String>, bool)> {
        out.iter()
            .map(|i| match i.interface() {
                generator::InterfaceVariant::PeerEmittedTopic { topic, peer } => {
                    (peer.module_path_for(&topic.name), true)
                }
                generator::InterfaceVariant::PeerConsumedTopic { topic, peer } => {
                    (peer.module_path_for(&topic.name), false)
                }
                other => panic!("expected a peer topic variant, got {other:?}"),
            })
            .collect()
    }

    fn coverage(err: PairingError) -> Vec<PairingCoverageMismatch> {
        match err {
            PairingError::Coverage(mismatches) => mismatches,
            PairingError::Other(reason) => panic!("expected a coverage diff, got: {reason}"),
        }
    }

    #[test]
    fn exact_emit_coverage_with_full_consume_resolves_both_directions() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }],
            } }"#,
        );
        let out = collect(&arm_manifest(), &cfg, &dirs).expect("exact coverage resolves");
        assert_eq!(
            peer_modules(&out),
            vec![
                (
                    vec!["controller".to_string(), "joint_states".to_string()],
                    true
                ),
                (
                    vec!["controller".to_string(), "joint_commands".to_string()],
                    false
                ),
            ]
        );
    }

    /// The shape and QoS of a pairing topic come from the document, never
    /// from the manifest, which carries only `{link_id, name}`.
    #[test]
    fn resolved_topics_carry_the_documents_shape_and_qos() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: { emits: [{ link_id: "controller", name: "joint_states" }] } }"#,
        );
        let out = collect(&arm_manifest(), &cfg, &dirs).expect("resolves");
        let generator::InterfaceVariant::PeerEmittedTopic { topic, peer } = out[0].interface()
        else {
            panic!("expected a peer-emitted topic");
        };
        assert_eq!(topic.name, "joint_states");
        assert_eq!(peer.pairing_name, "arm_link");
        assert_eq!(peer.pairing_tag, "v1");
        assert_eq!(peer.link_id, "controller");
    }

    #[test]
    fn missing_emit_entry_is_rejected_naming_the_topic() {
        let (_t, _c, dirs) = seeded_dirs();
        let err = collect(&arm_manifest(), &config::node::Interfaces::default(), &dirs)
            .expect_err("an unlisted emitted topic is a coverage failure");
        let mismatches = coverage(err);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].missing_emits, vec!["joint_states"]);
        assert_eq!(mismatches[0].link_id, "controller");
        assert!(
            mismatches[0].to_string().contains("joint_states"),
            "rendered error must name the missing topic: {}",
            mismatches[0]
        );
    }

    /// Consume coverage is free: a slot may declare no consumes at all, and
    /// only the emit side is checked against the document.
    #[test]
    fn zero_consume_entries_is_legal() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: { emits: [{ link_id: "controller", name: "joint_states" }] } }"#,
        );
        let out = collect(&arm_manifest(), &cfg, &dirs).expect("partial consume coverage is legal");
        assert_eq!(
            peer_modules(&out),
            vec![(
                vec!["controller".to_string(), "joint_states".to_string()],
                true
            )],
            "omitting a consume entry generates no module for that topic"
        );
    }

    #[test]
    fn unknown_emit_name_is_rejected() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: { emits: [
                { link_id: "controller", name: "joint_states" },
                { link_id: "controller", name: "not_in_doc" },
            ] } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(mismatches[0].unknown_emits, vec!["not_in_doc"]);
    }

    #[test]
    fn duplicate_emit_entry_is_rejected() {
        let (_t, _c, dirs) = seeded_dirs();
        // Parse-time dedup keys on (link_id, name), so a duplicate only
        // reaches here when assembled programmatically; the coverage check is
        // the backstop either way.
        let cfg = interfaces(
            r#"{ topics: { emits: [
                { link_id: "controller", name: "joint_states" },
                { link_id: "controller", name: "joint_states" },
            ] } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(mismatches[0].duplicated_emits, vec!["joint_states"]);
    }

    /// Listing a counterpart-role topic under `emits` claims to produce
    /// something the peer produces, so the error names the real emitter.
    #[test]
    fn emit_entry_naming_a_counterpart_role_topic_is_rejected() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: { emits: [
                { link_id: "controller", name: "joint_states" },
                { link_id: "controller", name: "joint_commands" },
            ] } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(
            mismatches[0].wrong_role_emits,
            vec!["joint_commands (emitted by controller)"],
            "the error must state which role emits it"
        );
    }

    #[test]
    fn consume_entry_naming_an_own_role_topic_is_rejected() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_states" }],
            } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(
            mismatches[0].wrong_role_consumes,
            vec!["joint_states (emitted by this node's role arm)"]
        );
    }

    #[test]
    fn consume_entry_absent_from_the_document_is_rejected() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "typo" }],
            } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(mismatches[0].unknown_consumes, vec!["typo"]);
        let rendered = mismatches[0].to_string();
        for needle in ["typo", "controller", "arm_link", "v1"] {
            assert!(
                rendered.contains(needle),
                "error must name the entry, the slot and the document, missing {needle}: {rendered}"
            );
        }
    }

    /// Several wrong entries on one slot produce one aggregated diff rather
    /// than a cascade of one error per entry.
    #[test]
    fn several_wrong_emit_entries_produce_one_aggregated_diff() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: { emits: [
                { link_id: "controller", name: "joint_commands" },
                { link_id: "controller", name: "nope" },
                { link_id: "controller", name: "also_nope" },
            ] } }"#,
        );
        let mismatches = coverage(collect(&arm_manifest(), &cfg, &dirs).expect_err("rejected"));
        assert_eq!(mismatches.len(), 1, "one diff per slot: {mismatches:?}");
        assert_eq!(mismatches[0].missing_emits, vec!["joint_states"]);
        assert_eq!(mismatches[0].unknown_emits, vec!["nope", "also_nope"]);
        assert_eq!(
            mismatches[0].wrong_role_emits,
            vec!["joint_commands (emitted by controller)"]
        );
    }

    #[test]
    fn two_slots_with_one_broken_names_only_the_broken_slot() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_c, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let m = manifest(
            r#"{
                name: "commander", tag: "v1",
                depends_on: { pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "left" },
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "right" },
                ] }
            }"#,
        );
        let cfg =
            interfaces(r#"{ topics: { emits: [{ link_id: "left", name: "joint_commands" }] } }"#);
        let mismatches = coverage(collect(&m, &cfg, &dirs).expect_err("right slot is uncovered"));
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].link_id, "right");
        assert_eq!(mismatches[0].missing_emits, vec!["joint_commands"]);
    }

    /// Two slots of the same document each need their own full coverage, and
    /// each generates its own nested module directory.
    #[test]
    fn two_slots_of_one_document_resolve_independently() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_c, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let m = manifest(
            r#"{
                name: "commander", tag: "v1",
                depends_on: { pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "left" },
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "right" },
                ] }
            }"#,
        );
        let cfg = interfaces(
            r#"{ topics: {
                emits: [
                    { link_id: "left", name: "joint_commands" },
                    { link_id: "right", name: "joint_commands" },
                ],
                consumes: [{ link_id: "left", name: "joint_states" }],
            } }"#,
        );
        let out = collect(&m, &cfg, &dirs).expect("both slots covered");
        assert_eq!(
            peer_modules(&out),
            vec![
                (vec!["left".to_string(), "joint_commands".to_string()], true),
                (vec!["left".to_string(), "joint_states".to_string()], false),
                (
                    vec!["right".to_string(), "joint_commands".to_string()],
                    true
                ),
            ],
            "each slot nests under its own link_id, both directions together; \
             `right` legitimately has no consume module"
        );
    }

    /// `optional` governs whether the slot must be paired at launch, not what
    /// the node declares about it.
    #[test]
    fn optional_slot_is_coverage_checked_identically() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_c, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let m = manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: { pairings: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true },
                ] }
            }"#,
        );
        let mismatches = coverage(
            collect(&m, &config::node::Interfaces::default(), &dirs)
                .expect_err("an optional slot still needs exact emit coverage"),
        );
        assert_eq!(mismatches[0].missing_emits, vec!["joint_states"]);
    }

    /// Entries naming other slot kinds belong to the implements and consumed
    /// collectors; the pairing collector must step over them.
    #[test]
    fn entries_for_other_slots_are_ignored() {
        let (_t, _c, dirs) = seeded_dirs();
        let cfg = interfaces(
            r#"{ topics: {
                emits: [
                    { link_id: "controller", name: "joint_states" },
                    { name: "native_telemetry" },
                ],
                consumes: [{ link_id: "some_node_dep", name: "unrelated" }],
            } }"#,
        );
        let out = collect(&arm_manifest(), &cfg, &dirs).expect("unrelated entries are not ours");
        assert_eq!(
            peer_modules(&out),
            vec![(
                vec!["controller".to_string(), "joint_states".to_string()],
                true
            )]
        );
    }

    #[test]
    fn no_pairing_slots_yields_no_interfaces() {
        let (_t, _c, dirs) = seeded_dirs();
        let m = manifest(r#"{ name: "plain", tag: "v1" }"#);
        let out = collect(&m, &config::node::Interfaces::default(), &dirs).expect("ok");
        assert!(out.is_empty());
    }

    #[test]
    fn resolves_valid_role_and_returns_doc() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_t, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let m = manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]
                }
            }"#,
        );
        let docs = validate_pairing_specs(&m, &dirs, &|_| {}).expect("valid role resolves");
        let doc = docs.get("controller").expect("doc keyed by link_id");
        assert_eq!(doc.manifest.name.as_str(), "arm_link");
        assert!(doc.has_role("arm") && doc.has_role("controller"));
    }

    #[test]
    fn undeclared_role_is_rejected_naming_the_declared_roles() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_t, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let m = manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "arm_link", tag: "v1", role: "gripper", link_id: "controller" }]
                }
            }"#,
        );
        let err = validate_pairing_specs(&m, &dirs, &|_| {}).expect_err("bad role rejected");
        assert!(
            err.contains("gripper") && err.contains("controller") && err.contains("arm"),
            "error should name the bad role and the declared roles: {err}"
        );
    }

    #[test]
    fn cache_miss_suggests_repo_refresh() {
        let (_t, dirs) = make_peppy_dirs_with_cache(&[]);
        let m = manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]
                }
            }"#,
        );
        let err = validate_pairing_specs(&m, &dirs, &|_| {}).expect_err("miss must error");
        assert!(
            err.contains("arm_link:v1") && err.contains("peppy repo refresh"),
            "error should name the entry and suggest refresh: {err}"
        );
    }

    #[test]
    fn sha_pin_mismatch_and_drift_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let good_sha = entry.sha256.clone();
        let path = entry.path.clone();
        let (_t, dirs) = make_peppy_dirs_with_cache(&[entry]);

        // Pin to a sha not in the cache.
        let err = resolve_pairing_doc(&dirs, "arm_link", "v1", Some("beef"), &|_| {})
            .expect_err("unknown pin rejected");
        assert!(err.contains("beef"), "error: {err}");

        // Drift: rewrite the file; the cached fingerprint no longer matches.
        fs::write(
            &path,
            ARM_LINK_BODY.replace("joint_states", "joint_states_v2"),
        )
        .unwrap();
        let err = resolve_pairing_doc(&dirs, "arm_link", "v1", Some(&good_sha), &|_| {})
            .expect_err("drift rejected");
        assert!(err.contains("drifted"), "error: {err}");
    }
}
