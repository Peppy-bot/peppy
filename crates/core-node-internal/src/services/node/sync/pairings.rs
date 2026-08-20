//! Pairing-document resolution for `depends_on.pairings` and
//! `depends_on.pairing_observers`: loading the
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
    resolve_pairing_doc_cached(&cache, peppy_dirs, name, tag, sha256_pin, None, on_feedback)
}

/// Resolves one pairing document against an already-loaded cache, so
/// multi-slot validation doesn't re-read `pairings.json5` per entry.
///
/// A launch pin fixes the bytes: this machine reuses its own copy on a
/// content match and fetches the pin's origin otherwise, but its own
/// priority rules never pick the document. Without one, the local rules
/// apply, with the manifest's own optional sha pin.
fn resolve_pairing_doc_cached(
    cache: &[repo_cache::PairingCacheEntry],
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PeppyPairing, String> {
    let parse = |content: &str| {
        daemon_config::pairing::PeppyPairingParser::from_content(content).map_err(|e| e.to_string())
    };
    if let Some(pins) = doc_pins {
        let pin = pins.require(
            daemon_config::repository::PinKind::Pairing,
            name,
            tag,
            sha256_pin,
        )?;
        return repo_cache::resolve_pinned_doc(peppy_dirs, cache, pin, parse, on_feedback);
    }
    repo_cache::resolve_cached_doc(peppy_dirs, cache, name, tag, sha256_pin, parse, on_feedback)
}

/// Validates every pairing slot of a manifest, participant and observer alike,
/// against its resolved pairing document and returns the resolved docs keyed by
/// slot link_id (ready for the codegen collection step):
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
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<HashMap<String, PeppyPairing>, String> {
    let slots = pairing_slots(manifest);
    if slots.is_empty() {
        return Ok(HashMap::new());
    }

    let cache = repo_cache::load_pairing_cache(peppy_dirs)
        .map_err(|e| format!("failed to load pairing cache: {e}"))?;

    // Two slots referencing the same document (e.g. a commander driving two
    // arms over the same pairing) resolve it once, not once per slot.
    let mut resolved: HashMap<(&str, &str, Option<&str>), PeppyPairing> = HashMap::new();
    let mut out = HashMap::new();
    for slot in slots {
        let doc = match resolved.entry((slot.name, slot.tag, slot.sha256)) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => v.insert(resolve_pairing_doc_cached(
                &cache,
                peppy_dirs,
                slot.name,
                slot.tag,
                slot.sha256,
                doc_pins,
                on_feedback,
            )?),
        };
        if !doc.has_role(slot.role) {
            let declared: Vec<&str> = doc.roles.iter().map(|r| r.as_str()).collect();
            return Err(format!(
                "pairing slot `{}`: role `{}` is not declared by pairing `{}:{}` \
                 (declared roles: [{}])",
                slot.link_id,
                slot.role,
                slot.name,
                slot.tag,
                declared.join(", "),
            ));
        }
        out.insert(slot.link_id.to_string(), doc.clone());
    }
    Ok(out)
}

/// One pairing slot flattened to the fields document resolution needs. The two
/// slot lists differ only in what `role` refers to (played vs observed), and
/// that distinction does not reach the document lookup or the role check.
struct PairingSlotRef<'a> {
    name: &'a str,
    tag: &'a str,
    sha256: Option<&'a str>,
    link_id: &'a str,
    role: &'a str,
}

/// Every pairing slot of a manifest, participants first then observers.
fn pairing_slots(manifest: &config::node::Manifest) -> Vec<PairingSlotRef<'_>> {
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return Vec::new();
    };
    let participants = depends_on.pairings.iter().map(|p| PairingSlotRef {
        name: p.name.as_str(),
        tag: &p.tag,
        sha256: p.sha256.as_deref(),
        link_id: &p.link_id,
        role: &p.role,
    });
    let observers = depends_on.pairing_observers.iter().map(|o| PairingSlotRef {
        name: o.name.as_str(),
        tag: &o.tag,
        sha256: o.sha256.as_deref(),
        link_id: &o.link_id,
        role: &o.role,
    });
    participants.chain(observers).collect()
}

/// A failure resolving a manifest's pairing slots against the declared interface
/// entries. Mirrors `ImplementsError`: coverage problems are aggregated per
/// slot so a node with several wrong entries produces one readable report.
#[derive(Debug)]
pub enum PairingError {
    /// One aggregated set-diff per broken slot.
    Coverage(Vec<PairingCoverageMismatch>),
    /// One aggregated problem list per entry whose `refine` block the
    /// pairing document does not admit. Reported only once coverage is
    /// clean.
    Refinement(Vec<config::RefinementMismatch>),
    Other(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coverage(mismatches) => config::write_joined(f, mismatches, "; "),
            Self::Refinement(mismatches) => config::write_joined(f, mismatches, "; "),
            Self::Other(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for PairingError {}

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

/// Resolves every pairing slot, participant and observer, into the generator inputs for the
/// `paired_topics/<link_id>/<topic>` modules, driven by what the node declares:
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
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<generator::DeploymentInterface>, PairingError> {
    let Some(depends_on) = manifest.depends_on.as_ref() else {
        return Ok(Vec::new());
    };
    if depends_on.pairings.is_empty() && depends_on.pairing_observers.is_empty() {
        return Ok(Vec::new());
    }
    let docs = validate_pairing_specs(manifest, peppy_dirs, doc_pins, on_feedback)?;
    let doc_of = |link_id: &str| {
        docs.get(link_id)
            .expect("validate_pairing_specs returns a doc per declared slot")
    };
    let context_of =
        |name: &str, tag: &str, link_id: &str, optional: bool| generator::PeerContext {
            link_id: link_id.to_string(),
            pairing_name: name.to_string(),
            pairing_tag: tag.to_string(),
            optional,
        };

    let mut out = Vec::new();
    let mut broken = Vec::new();
    let mut inadmissible = Vec::new();
    for participant in &depends_on.pairings {
        let context = context_of(
            participant.name.as_str(),
            &participant.tag,
            &participant.link_id,
            participant.optional,
        );
        let mismatch = collect_participant_slot(
            participant,
            doc_of(&participant.link_id),
            interfaces_cfg,
            &context,
            &mut out,
            &mut inadmissible,
        );
        if !mismatch.is_empty() {
            broken.push(mismatch);
        }
    }
    for observer in &depends_on.pairing_observers {
        // Observer vacancy is expressed through cardinality, never `optional`.
        let context = context_of(
            observer.name.as_str(),
            &observer.tag,
            &observer.link_id,
            false,
        );
        let mismatch = collect_observer_slot(
            observer,
            doc_of(&observer.link_id),
            interfaces_cfg,
            &context,
            &mut out,
            &mut inadmissible,
        );
        if !mismatch.is_empty() {
            broken.push(mismatch);
        }
    }

    if !broken.is_empty() {
        return Err(PairingError::Coverage(broken));
    }
    if !inadmissible.is_empty() {
        return Err(PairingError::Refinement(inadmissible));
    }
    Ok(out)
}

/// Resolves a participant slot: `topics.emits` become peer-emitted (exact
/// coverage against the role's topics), `topics.consumes` become peer-consumed
/// (partial coverage against the counterpart role's topics). An entry whose
/// `refine` block the topic does not admit counts toward coverage (the topic
/// exists) and lands in `inadmissible` instead of `out`.
fn collect_participant_slot(
    participant: &config::node::PairingParticipantDependency,
    doc: &PeppyPairing,
    interfaces_cfg: &config::node::Interfaces,
    context: &generator::PeerContext,
    out: &mut Vec<generator::DeploymentInterface>,
    inadmissible: &mut Vec<config::RefinementMismatch>,
) -> PairingCoverageMismatch {
    let role = participant.role.as_str();
    let mut coverage = SlotCoverage::default();
    let mismatch_for = |name: &str, problems| {
        config::RefinementMismatch::for_pairing(
            (participant.name.as_str(), &participant.tag),
            &participant.link_id,
            name,
            problems,
        )
    };

    for (name, refine) in declared_topics(interfaces_cfg, &participant.link_id, Direction::Emits) {
        let Some(topic) = doc.topics.iter().find(|t| t.name == name) else {
            coverage.unknown_emits.push(name.to_string());
            continue;
        };
        if topic.emitted_by != role {
            coverage
                .wrong_role_emits
                .push(format!("{name} (emitted by {})", topic.emitted_by));
            continue;
        }
        *coverage.visited_emits.entry(name.to_string()).or_insert(0) += 1;
        match native_topic(topic, refine) {
            Ok(native) => out.push(generator::DeploymentInterface::peer_emitted_topic(
                native,
                context.clone(),
            )),
            Err(problems) => inadmissible.push(mismatch_for(name, problems)),
        }
    }

    for (name, refine) in declared_topics(interfaces_cfg, &participant.link_id, Direction::Consumes)
    {
        let Some(topic) = doc.topics.iter().find(|t| t.name == name) else {
            coverage.unknown_consumes.push(name.to_string());
            continue;
        };
        if topic.emitted_by == role {
            coverage
                .wrong_role_consumes
                .push(format!("{name} (emitted by this node's role {role})"));
            continue;
        }
        *coverage
            .visited_consumes
            .entry(name.to_string())
            .or_insert(0) += 1;
        match native_topic(topic, refine) {
            Ok(native) => out.push(generator::DeploymentInterface::peer_consumed_topic(
                native,
                context.clone(),
            )),
            Err(problems) => inadmissible.push(mismatch_for(name, problems)),
        }
    }

    build_participant_mismatch(participant, doc, coverage)
}

/// Resolves an observer slot: it emits nothing (any `topics.emits` entry is an
/// error), and each `topics.consumes` entry taps a topic emitted BY the
/// observed role, becoming an observed topic. Consume coverage is partial, like
/// a participant's, and an inadmissible `refine` block is handled the same way.
fn collect_observer_slot(
    observer: &config::node::PairingObserverDependency,
    doc: &PeppyPairing,
    interfaces_cfg: &config::node::Interfaces,
    context: &generator::PeerContext,
    out: &mut Vec<generator::DeploymentInterface>,
    inadmissible: &mut Vec<config::RefinementMismatch>,
) -> PairingCoverageMismatch {
    let observed_role = observer.role.as_str();
    let mut coverage = SlotCoverage::default();
    let mismatch_for = |name: &str, problems| {
        config::RefinementMismatch::for_pairing(
            (observer.name.as_str(), &observer.tag),
            &observer.link_id,
            name,
            problems,
        )
    };

    // An observer produces nothing; a stray emit entry naming its slot is a
    // hard error rather than a role mismatch.
    for (name, _) in declared_topics(interfaces_cfg, &observer.link_id, Direction::Emits) {
        coverage
            .wrong_role_emits
            .push(format!("{name} (an observer slot emits nothing)"));
    }

    for (name, refine) in declared_topics(interfaces_cfg, &observer.link_id, Direction::Consumes) {
        let Some(topic) = doc.topics.iter().find(|t| t.name == name) else {
            coverage.unknown_consumes.push(name.to_string());
            continue;
        };
        // An observer consumes the OBSERVED role's own topics, the opposite
        // direction from a participant.
        if topic.emitted_by != observed_role {
            coverage.wrong_role_consumes.push(format!(
                "{name} (emitted by {}, not the observed role {observed_role})",
                topic.emitted_by
            ));
            continue;
        }
        *coverage
            .visited_consumes
            .entry(name.to_string())
            .or_insert(0) += 1;
        match native_topic(topic, refine) {
            Ok(native) => out.push(generator::DeploymentInterface::observed_topic(
                native,
                context.clone(),
                observer.cardinality,
            )),
            Err(problems) => inadmissible.push(mismatch_for(name, problems)),
        }
    }

    build_observer_mismatch(observer, coverage)
}

/// Which direction of `interfaces.topics` an entry walk is reading.
#[derive(Clone, Copy)]
enum Direction {
    Emits,
    Consumes,
}

/// The entries declared for one pairing slot in one direction, in manifest
/// order, as `(name, refine)`. Entries naming other slots (contract-backed
/// emits, node-backed consumes) belong to the implements and consumed
/// collectors and are skipped here, which is what keeps pairing topics out of
/// `emitted_topics` and `consumed_topics`.
fn declared_topics<'a>(
    interfaces_cfg: &'a config::node::Interfaces,
    link_id: &'a str,
    direction: Direction,
) -> Vec<(&'a str, Option<&'a config::node::TopicRefinement>)> {
    let Some(topics) = interfaces_cfg.topics.as_ref() else {
        return Vec::new();
    };
    match direction {
        Direction::Emits => topics
            .emits
            .iter()
            .flatten()
            .filter_map(|e| e.as_linked())
            .filter(|e| e.link_id == link_id)
            .map(|e| (e.name.as_str(), e.refine.as_deref()))
            .collect(),
        Direction::Consumes => topics
            .consumes
            .iter()
            .flatten()
            .filter(|c| c.link_id == link_id)
            .map(|c| (c.name.as_str(), c.refine.as_deref()))
            .collect(),
    }
}

/// The generator carries a pairing topic's shape in a `NativeEmittedTopic` in
/// both directions; the document is the sole source of that shape, and the
/// entry's `refine` block may pin the length of arrays it leaves generic.
fn native_topic(
    topic: &daemon_config::pairing::PairingTopic,
    refine: Option<&config::node::TopicRefinement>,
) -> Result<config::node::NativeEmittedTopic, Vec<config::node::RefinementProblem>> {
    config::node::refined(
        refine,
        config::node::NativeEmittedTopic {
            name: topic.name.clone(),
            qos_profile: topic.qos_profile.clone(),
            message_format: topic.message_format.clone(),
        },
    )
}

/// Turns one slot's bookkeeping into its aggregated diff. Only the emit side
/// contributes `missing`/`duplicated` against the document: consume coverage
/// is free, so an unlisted counterpart topic is not a defect, but naming one
/// twice still is.
fn build_participant_mismatch(
    dep: &config::node::PairingParticipantDependency,
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
    let duplicated_consumes = duplicated_consumes(&coverage);

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

/// An observer has no emit coverage (it emits nothing), so the only defects are
/// stray emit entries (surfaced as `wrong_role_emits`) and consume-side
/// unknown / wrong-role / duplicate entries. The `role` field carries the
/// observed role for the report.
fn build_observer_mismatch(
    dep: &config::node::PairingObserverDependency,
    coverage: SlotCoverage,
) -> PairingCoverageMismatch {
    let duplicated_consumes = duplicated_consumes(&coverage);

    PairingCoverageMismatch {
        pairing_name: dep.name.as_str().to_string(),
        pairing_tag: dep.tag.clone(),
        link_id: dep.link_id.clone(),
        role: dep.role.clone(),
        missing_emits: Vec::new(),
        unknown_emits: coverage.unknown_emits,
        duplicated_emits: Vec::new(),
        wrong_role_emits: coverage.wrong_role_emits,
        unknown_consumes: coverage.unknown_consumes,
        duplicated_consumes,
        wrong_role_consumes: coverage.wrong_role_consumes,
    }
}

/// The counterpart topics named more than once in `topics.consumes`, sorted.
fn duplicated_consumes(coverage: &SlotCoverage) -> Vec<String> {
    let mut out: Vec<String> = coverage
        .visited_consumes
        .iter()
        .filter(|(_, visits)| **visits > 1)
        .map(|(name, _)| name.clone())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
            pairing_name: daemon_config::repository::ItemName::parse(name).unwrap(),
            tag: daemon_config::repository::ItemTag::parse(tag).unwrap(),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(body.as_bytes()),
            origin: repo_cache::EntryOrigin::Fs { path: path.clone() },
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
        collect_pairing_interfaces(manifest, interfaces_cfg, dirs, None, &|_| {})
    }

    /// The `(module_path, is_emitted)` of each produced interface, so tests
    /// assert both the direction and the `paired_topics/<link_id>/<topic>` path.
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
            other => panic!("expected a coverage diff, got: {other}"),
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

    /// A pairing keeping its joint vectors generic: the leader streams
    /// setpoints, the follower streams its state back.
    const JOINT_LINK_BODY: &str = r#"{
        peppy_schema: "pairing/v1",
        manifest: { name: "joint_link", tag: "v1" },
        roles: ["leader", "follower"],
        topics: [
            {
                emitted_by: "leader",
                name: "joint_setpoints",
                message_format: { positions: { $type: "array", $items: "f64" } }
            },
            {
                emitted_by: "follower",
                name: "joint_states",
                message_format: { stamp: "time", positions: { $type: "array", $items: "f64" } }
            }
        ]
    }"#;

    fn joint_link_dirs() -> (TempDir, TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let entry = seed_pairing(tmp.path(), "joint_link", "v1", JOINT_LINK_BODY);
        let (cache_tmp, dirs) = make_peppy_dirs_with_cache(&[entry]);
        (tmp, cache_tmp, dirs)
    }

    fn positions_length(topic: &config::node::NativeEmittedTopic) -> Option<usize> {
        let format = topic
            .message_format
            .as_ref()
            .expect("the topic has a format");
        let config::node::SchemaType::Array(array) = &format.0["positions"] else {
            panic!("`positions` should be an array");
        };
        array.length
    }

    /// A follower that knows its joint count pins the generic vectors in
    /// both directions of its slot: the state it emits and the setpoints
    /// it consumes.
    #[test]
    fn refine_pins_generic_arrays_in_both_directions_of_a_participant_slot() {
        let (_t, _c, dirs) = joint_link_dirs();
        let follower = manifest(
            r#"{
                name: "seven_dof_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "joint_link", tag: "v1", role: "follower", link_id: "leader" }]
                }
            }"#,
        );
        let cfg = interfaces(
            r#"{ topics: {
                emits: [{ link_id: "leader", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } }],
                consumes: [{ link_id: "leader", name: "joint_setpoints", refine: { message_format: { positions: { $length: 7 } } } }],
            } }"#,
        );
        let out = collect(&follower, &cfg, &dirs).expect("admissible refinements resolve");
        assert_eq!(out.len(), 2);
        for resolved in &out {
            match resolved.interface() {
                generator::InterfaceVariant::PeerEmittedTopic { topic, .. }
                | generator::InterfaceVariant::PeerConsumedTopic { topic, .. } => {
                    assert_eq!(positions_length(topic), Some(7), "{}", topic.name);
                }
                other => panic!("expected a peer topic variant, got {other:?}"),
            }
        }
    }

    /// An observer taps the observed role's topics and may pin them the
    /// same way.
    #[test]
    fn refine_pins_generic_arrays_of_an_observed_topic() {
        let (_t, _c, dirs) = joint_link_dirs();
        let recorder = manifest(
            r#"{
                name: "recorder", tag: "v1",
                depends_on: {
                    pairing_observers: [{ name: "joint_link", tag: "v1", role: "follower", link_id: "watch" }]
                }
            }"#,
        );
        let cfg = interfaces(
            r#"{ topics: {
                consumes: [{ link_id: "watch", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } }],
            } }"#,
        );
        let out = collect(&recorder, &cfg, &dirs).expect("admissible refinements resolve");
        assert_eq!(out.len(), 1);
        let generator::InterfaceVariant::ObservedTopic { topic, .. } = out[0].interface() else {
            panic!("expected an observed topic, got {:?}", out[0]);
        };
        assert_eq!(positions_length(topic), Some(7));
    }

    /// An inadmissible pin is reported for its entry once the slot's
    /// coverage is clean, naming the pairing the slot resolved through;
    /// while coverage is broken, the coverage diff is what gets reported.
    #[test]
    fn inadmissible_refine_is_reported_after_coverage() {
        let (_t, _c, dirs) = joint_link_dirs();
        let follower = manifest(
            r#"{
                name: "seven_dof_arm", tag: "v1",
                depends_on: {
                    pairings: [{ name: "joint_link", tag: "v1", role: "follower", link_id: "leader" }]
                }
            }"#,
        );
        let bad_emit = r#"{ link_id: "leader", name: "joint_states", refine: { message_format: { stamp: { $length: 1 } } } }"#;

        let cfg = interfaces(&format!(r#"{{ topics: {{ emits: [{bad_emit}] }} }}"#));
        let err = collect(&follower, &cfg, &dirs).expect_err("a `time` field has no length to pin");
        let PairingError::Refinement(mismatches) = err else {
            panic!("expected a refinement report, got: {err}");
        };
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].document, "pairing `joint_link:v1`");
        assert_eq!(mismatches[0].link_id, "leader");
        assert_eq!(mismatches[0].name, "joint_states");
        assert_eq!(mismatches[0].problems.len(), 1);
        assert_eq!(mismatches[0].problems[0].path, "message_format.stamp");
        assert!(
            mismatches[0]
                .to_string()
                .contains("the document declares a `time`"),
            "{}",
            mismatches[0]
        );

        // The same entry plus a consume entry naming no topic of the pairing:
        // the coverage diff is reported, the refinement report waits.
        let cfg = interfaces(&format!(
            r#"{{ topics: {{
                emits: [{bad_emit}],
                consumes: [{{ link_id: "leader", name: "joint_torques" }}],
            }} }}"#
        ));
        let mismatches = coverage(
            collect(&follower, &cfg, &dirs).expect_err("an unknown consume is a coverage failure"),
        );
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].unknown_consumes, vec!["joint_torques"]);
        assert!(
            mismatches[0].missing_emits.is_empty(),
            "the refined emit counts as visiting its topic: {:?}",
            mismatches[0]
        );
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

    /// Whether a slot ends up paired at launch has no bearing on what the node
    /// must declare about it: the role's topics are covered or the manifest is
    /// wrong.
    #[test]
    fn a_participant_slot_declaring_no_interfaces_is_coverage_checked() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_pairing(tmp.path(), "arm_link", "v1", ARM_LINK_BODY);
        let (_c, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let m = manifest(
            r#"{
                name: "robot_arm", tag: "v1",
                depends_on: { pairings: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "controller" },
                ] }
            }"#,
        );
        let mismatches = coverage(
            collect(&m, &config::node::Interfaces::default(), &dirs)
                .expect_err("a declared slot needs exact emit coverage"),
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
        let docs = validate_pairing_specs(&m, &dirs, None, &|_| {}).expect("valid role resolves");
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
        let err = validate_pairing_specs(&m, &dirs, None, &|_| {}).expect_err("bad role rejected");
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
        let err = validate_pairing_specs(&m, &dirs, None, &|_| {}).expect_err("miss must error");
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
        let path = entry.origin.path_str().to_owned();
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
        let err = resolve_pairing_doc(&dirs, "arm_link", "v1", Some(good_sha.as_str()), &|_| {})
            .expect_err("drift rejected");
        assert!(err.contains("drifted"), "error: {err}");
    }
}
