use super::deps::{
    DependencyKind, DependencyLookupEntry, DependencyOfferings, build_dependency_lookup,
    build_dependency_offerings,
};
use crate::services::repo::cache as repo_cache;
use config::ContractCoverageMismatch;
use config::node::InterfaceKind;
use daemon_config::consts::PeppyDirs;
use generator::{ConsumedActionMessage, ContractOrigin, DeploymentInterface};
use node_stack::NodeStack;
use std::collections::HashMap;

/// Collects consumed interfaces from a node config and resolves their message
/// formats by looking up the exposed interfaces from dependency nodes via the
/// caller-supplied resolver.
///
/// The `resolve` closure returns a [`config::node::NodeConfig`] for a given
/// `(name, tag)` pair, or `None` if the dependency cannot be found. Callers
/// usually wrap a [`NodeStack`] (see [`stack_resolver`]) but can also chain a
/// peer map first to resolve sibling nodes that haven't been added to the
/// stack yet; used by `node sync -a` for batch operations.
/// The full peppygen interface set for one node, in one call: consumed
/// interfaces from `depends_on`, contract-backed produced entries resolved
/// through `manifest.implements` (coverage-checked, sha-pinned,
/// drift-checked), and both directions of every declared pairing slot
/// (role-validated, sha-pinned, drift-checked). Used by `node add`,
/// `node sync`, and the launch-time auto-sync so the "what feeds peppygen"
/// sequence lives in one place. Error strings name the failing step; callers
/// only add their transport wrapper.
pub fn collect_all_deployment_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    resolve: impl Fn(&str, &str) -> Option<config::node::NodeConfig>,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces =
        collect_consumed_interfaces(manifest, interfaces_cfg, resolve, peppy_dirs, on_feedback)
            .map_err(|reason| format!("failed to resolve consumed interfaces: {reason}"))?;
    interfaces.extend(
        resolve_implements(manifest, interfaces_cfg, peppy_dirs, on_feedback)
            .map_err(|reason| format!("failed to resolve `manifest.implements`: {reason}"))?,
    );
    interfaces.extend(
        super::pairings::collect_pairing_interfaces(manifest, peppy_dirs, on_feedback)
            .map_err(|reason| format!("failed to resolve `depends_on.pairings`: {reason}"))?,
    );
    Ok(interfaces)
}

pub fn collect_consumed_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    resolve: impl Fn(&str, &str) -> Option<config::node::NodeConfig>,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces = Vec::new();
    let dep_lookup = build_dependency_lookup(manifest);

    // Pre-resolve each unique node dependency into a per-dep offerings table
    // of its NATIVE emits/exposes. Contract-backed entries are deliberately
    // absent: node dependencies expose native interfaces only, and
    // contract-backed interfaces are consumed through `depends_on.contracts`.
    let mut node_dep_offerings: HashMap<(String, String), DependencyOfferings> = HashMap::new();
    // Memoized parsed contracts for `depends_on.contracts`
    // entries, keyed by `link_id` so two entries with the same
    // `(name, tag)` but different sha256 pins are cached and resolved
    // separately. `resolve_contract_doc` handles SHA-pin matching and
    // on-disk drift detection per load.
    let mut contract_dep_docs: HashMap<String, daemon_config::contract::PeppyContract> =
        HashMap::new();

    for (link_id, entry) in dep_lookup.iter() {
        match entry.kind {
            DependencyKind::Node => {
                let node_key = (entry.name.clone(), entry.tag.clone());
                if node_dep_offerings.contains_key(&node_key) {
                    continue;
                }
                let Some(dep_config) = resolve(&entry.name, &entry.tag) else {
                    continue;
                };
                node_dep_offerings.insert(node_key, build_dependency_offerings(&dep_config));
            }
            DependencyKind::Contract => {
                if contract_dep_docs.contains_key(link_id) {
                    continue;
                }
                let parsed = resolve_contract_doc(
                    peppy_dirs,
                    &entry.name,
                    &entry.tag,
                    entry.sha256.as_deref(),
                    on_feedback,
                )?;
                contract_dep_docs.insert(link_id.clone(), parsed);
            }
        }
    }

    if let Some(topic_interfaces) = &interfaces_cfg.topics
        && let Some(consumed_topics) = &topic_interfaces.consumes
    {
        for consumed_topic in consumed_topics {
            let Some((message_format, dependency)) = resolve_consumed_offering(
                &dep_lookup,
                &node_dep_offerings,
                &contract_dep_docs,
                &consumed_topic.link_id,
                consumed_topic.name.trim(),
                |offerings, name| offerings.topics.get(name).cloned(),
                |parsed, name| {
                    parsed
                        .interfaces
                        .topics
                        .iter()
                        .find(|t| t.name.trim() == name)
                        .and_then(|emitted| emitted.message_format.clone())
                },
            ) else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_topic(
                consumed_topic.clone(),
                message_format,
                dependency,
            ));
        }
    }

    if let Some(service_interfaces) = &interfaces_cfg.services
        && let Some(consumed_services) = &service_interfaces.consumes
    {
        for consumed_service in consumed_services {
            let Some(((request_format, response_format), dependency)) = resolve_consumed_offering(
                &dep_lookup,
                &node_dep_offerings,
                &contract_dep_docs,
                &consumed_service.link_id,
                consumed_service.name.trim(),
                |offerings, name| offerings.services.get(name).cloned(),
                |parsed, name| {
                    let exposed = parsed
                        .interfaces
                        .services
                        .iter()
                        .find(|s| s.name.trim() == name)?;
                    let request_format = exposed.request_message_format.clone().unwrap_or_default();
                    let response_format =
                        exposed.response_message_format.clone().unwrap_or_default();
                    Some((request_format, response_format))
                },
            ) else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_service(
                consumed_service.clone(),
                request_format,
                response_format,
                dependency,
            ));
        }
    }

    if let Some(action_interfaces) = &interfaces_cfg.actions
        && let Some(consumed_actions) = &action_interfaces.consumes
    {
        for consumed_action in consumed_actions {
            let Some((action_message, dependency)) = resolve_consumed_offering(
                &dep_lookup,
                &node_dep_offerings,
                &contract_dep_docs,
                &consumed_action.link_id,
                consumed_action.name.trim(),
                |offerings, name| offerings.actions.get(name).cloned(),
                |parsed, name| {
                    parsed
                        .interfaces
                        .actions
                        .iter()
                        .find(|a| a.name.trim() == name)
                        .map(action_message_from_exposed)
                },
            ) else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_action(
                consumed_action.clone(),
                action_message,
                dependency,
            ));
        }
    }

    Ok(interfaces)
}

/// Resolves a single consumed interface to its message-format payload plus the
/// `DependencyContext` the generator needs to address it. Node dependencies
/// resolve against the producer's native offerings only (node-addressed);
/// contract dependencies resolve against the contract document
/// (contract-addressed).
fn resolve_consumed_offering<T>(
    dep_lookup: &HashMap<String, DependencyLookupEntry>,
    node_dep_offerings: &HashMap<(String, String), DependencyOfferings>,
    contract_dep_docs: &HashMap<String, daemon_config::contract::PeppyContract>,
    link_id: &str,
    lookup_name: &str,
    extract_from_node: impl FnOnce(&DependencyOfferings, &str) -> Option<T>,
    extract_from_contract: impl FnOnce(&daemon_config::contract::PeppyContract, &str) -> Option<T>,
) -> Option<(T, generator::DependencyContext)> {
    let entry = dep_lookup.get(link_id)?;
    match entry.kind {
        DependencyKind::Node => {
            let offerings = node_dep_offerings.get(&(entry.name.clone(), entry.tag.clone()))?;
            let extracted = extract_from_node(offerings, lookup_name)?;
            Some((
                extracted,
                generator::DependencyContext::native(&entry.name, &entry.tag, link_id),
            ))
        }
        DependencyKind::Contract => {
            let parsed = contract_dep_docs.get(link_id)?;
            let extracted = extract_from_contract(parsed, lookup_name)?;
            Some((
                extracted,
                generator::DependencyContext::contract(&entry.name, &entry.tag, link_id),
            ))
        }
    }
}

pub(super) fn action_message_from_exposed(
    exposed_action: &config::node::NativeExposedAction,
) -> ConsumedActionMessage {
    ConsumedActionMessage {
        goal_request: exposed_action
            .goal_service
            .as_ref()
            .and_then(|s| s.request_message_format.clone()),
        feedback: exposed_action
            .feedback_topic
            .as_ref()
            .and_then(|t| t.message_format.clone()),
        result_response: exposed_action
            .result_service
            .as_ref()
            .and_then(|s| s.response_message_format.clone()),
    }
}

/// Loads a `PeppyContract` document from the local contract cache for
/// `(name, tag)`, verifying both the SHA pin (when set) and on-disk drift
/// against the cached fingerprint. Returns the parsed contract document,
/// or an error string ready to surface to the client. Shared between
/// [`resolve_implements`] (producer side) and the `depends_on.contracts`
/// resolution path (consumer side).
pub(crate) fn resolve_contract_doc(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<daemon_config::contract::PeppyContract, String> {
    let cache = repo_cache::load_contract_cache(peppy_dirs)
        .map_err(|e| format!("failed to load contract cache: {e}"))?;

    let entry = match sha256_pin {
        Some(sha) => repo_cache::lookup_contract_by_sha256(&cache, name, tag, sha),
        None => repo_cache::lookup_contract(&cache, name, tag),
    };

    repo_cache::resolve_cached_doc(
        peppy_dirs,
        "contract",
        &format!("{name}:{tag}"),
        sha256_pin,
        entry.map(Into::into),
        |content| {
            daemon_config::contract::PeppyContractParser::from_content(content)
                .map_err(|e| e.to_string())
        },
        on_feedback,
    )
}

/// Error from [`resolve_implements`]. Tier B coverage failures keep their
/// per-slot [`ContractCoverageMismatch`] payloads so callers can render or
/// match each slot's diff individually; every other failure is a plain
/// message, matching the sync module's string error contract.
#[derive(Debug)]
pub enum ImplementsError {
    /// One aggregated set-diff per broken slot.
    Coverage(Vec<ContractCoverageMismatch>),
    Other(String),
}

impl std::fmt::Display for ImplementsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coverage(mismatches) => {
                let rendered: Vec<String> = mismatches.iter().map(ToString::to_string).collect();
                f.write_str(&rendered.join("; "))
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ImplementsError {}

impl From<String> for ImplementsError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

/// Per-slot Tier B coverage bookkeeping: which contract members the manifest
/// entries visited (with counts), plus names that matched no member of the
/// right kind.
#[derive(Default)]
struct SlotCoverage {
    visited: HashMap<(InterfaceKind, String), u32>,
    unknown: Vec<String>,
    wrong_kind: Vec<String>,
}

/// Resolves the node's contract-backed produced entries (Decision 1:
/// entries drive codegen). For every `{link_id, name}` entry in
/// `topics.emits` / `services.exposes` / `actions.exposes`:
///
/// entry -> implements slot -> contract document -> member (by name and
/// kind) -> shape/qos, stamped with a [`ContractOrigin`] so the generator
/// nests the artifact under `{contract_name}/{contract_tag}/{leaf}` and
/// embeds the matching wire segments.
///
/// After resolution, the Tier B coverage check runs per (slot x kind): the
/// contract-backed entries referencing a slot must cover every member of
/// its contract exactly once, with no extras. Any discrepancy is reported
/// as one aggregated set-diff per broken slot
/// ([`ContractCoverageMismatch`]).
///
/// Contract documents resolve through the local cache with sha256 pinning
/// and on-disk drift detection (see [`resolve_contract_doc`]); a cache miss
/// surfaces "run `peppy repo refresh`".
pub fn resolve_implements(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, ImplementsError> {
    if manifest.implements.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve each slot's contract document once, keeping the slot alongside
    // so entry resolution recovers both with one lookup. Parse-time validation
    // already rejected duplicate link_ids and duplicate (name, tag) pairs.
    let mut docs: HashMap<
        &str,
        (
            &config::node::ImplementsEntry,
            daemon_config::contract::PeppyContract,
        ),
    > = HashMap::new();
    for slot in &manifest.implements {
        let parsed = resolve_contract_doc(
            peppy_dirs,
            slot.name.as_str(),
            &slot.tag,
            slot.sha256.as_deref(),
            on_feedback,
        )?;
        docs.insert(slot.link_id.as_str(), (slot, parsed));
    }

    let mut out: Vec<DeploymentInterface> = Vec::new();
    let mut coverage: HashMap<&str, SlotCoverage> = HashMap::new();

    for (kind, entry) in interfaces_cfg.contract_backed_entries() {
        let name = entry.name.as_str();
        let Some((slot, doc)) = docs.get(entry.link_id.as_str()) else {
            // Parse-time validation guarantees every produced entry's
            // link_id names an implements slot; reaching this means the
            // config bypassed the parser.
            return Err(ImplementsError::Other(format!(
                "produced entry `{name}` references link_id `{}`, which matches no \
                 `manifest.implements` slot",
                entry.link_id
            )));
        };
        let origin = ContractOrigin {
            contract_name: slot.name.as_str().to_string(),
            contract_tag: slot.tag.clone(),
        };
        let slot_coverage = coverage.entry(slot.link_id.as_str()).or_default();

        let resolved = match kind {
            InterfaceKind::Topic => doc
                .interfaces
                .topics
                .iter()
                .find(|t| t.name == name)
                .map(|topic| DeploymentInterface::emitted_topic(topic.clone(), Some(origin))),
            InterfaceKind::Service => doc
                .interfaces
                .services
                .iter()
                .find(|s| s.name == name)
                .map(|service| DeploymentInterface::exposed_service(service.clone(), Some(origin))),
            InterfaceKind::Action => doc
                .interfaces
                .actions
                .iter()
                .find(|a| a.name == name)
                .map(|action| DeploymentInterface::exposed_action(action.clone(), Some(origin))),
        };

        match resolved {
            Some(interface) => {
                out.push(interface);
                *slot_coverage
                    .visited
                    .entry((kind, name.to_string()))
                    .or_insert(0) += 1;
            }
            None => {
                // The entry's name was not found under `kind`, so any kind
                // this returns is necessarily a different one.
                match member_kind(doc, name) {
                    Some(member_kind) => slot_coverage.wrong_kind.push(format!(
                        "{name} (declared as {kind}, contract declares {member_kind})"
                    )),
                    None => slot_coverage.unknown.push(name.to_string()),
                }
            }
        }
    }

    // Tier B coverage: per (slot x kind) set equality between the contract's
    // members and the visited entries, aggregated into one diff per slot.
    let mut broken: Vec<ContractCoverageMismatch> = Vec::new();
    for slot in &manifest.implements {
        let (_, doc) = &docs[slot.link_id.as_str()];
        let empty = SlotCoverage::default();
        let slot_coverage = coverage.get(slot.link_id.as_str()).unwrap_or(&empty);

        let mut missing: Vec<String> = Vec::new();
        let mut duplicated: Vec<String> = Vec::new();
        let members = doc
            .interfaces
            .topics
            .iter()
            .map(|t| (InterfaceKind::Topic, t.name.as_str()))
            .chain(
                doc.interfaces
                    .services
                    .iter()
                    .map(|s| (InterfaceKind::Service, s.name.as_str())),
            )
            .chain(
                doc.interfaces
                    .actions
                    .iter()
                    .map(|a| (InterfaceKind::Action, a.name.as_str())),
            );
        for (kind, member_name) in members {
            match slot_coverage
                .visited
                .get(&(kind, member_name.to_string()))
                .copied()
                .unwrap_or(0)
            {
                0 => missing.push(format!("{member_name} ({kind})")),
                1 => {}
                _ => duplicated.push(format!("{member_name} ({kind})")),
            }
        }

        if missing.is_empty()
            && duplicated.is_empty()
            && slot_coverage.unknown.is_empty()
            && slot_coverage.wrong_kind.is_empty()
        {
            continue;
        }
        broken.push(ContractCoverageMismatch {
            contract_name: slot.name.as_str().to_string(),
            contract_tag: slot.tag.clone(),
            link_id: slot.link_id.clone(),
            missing,
            unknown: slot_coverage.unknown.clone(),
            duplicated,
            wrong_kind: slot_coverage.wrong_kind.clone(),
        });
    }
    if !broken.is_empty() {
        return Err(ImplementsError::Coverage(broken));
    }

    Ok(out)
}

/// The first kind under which a contract declares a member named `name`.
fn member_kind(doc: &daemon_config::contract::PeppyContract, name: &str) -> Option<InterfaceKind> {
    if doc.interfaces.topics.iter().any(|t| t.name == name) {
        Some(InterfaceKind::Topic)
    } else if doc.interfaces.services.iter().any(|s| s.name == name) {
        Some(InterfaceKind::Service)
    } else if doc.interfaces.actions.iter().any(|a| a.name == name) {
        Some(InterfaceKind::Action)
    } else {
        None
    }
}

/// Convenience helper that builds a resolver closure backed by a [`NodeStack`].
///
/// Use this for callers that don't have any local peers to layer on top of
/// the daemon's persistent stack, i.e. `node add` and `auto_sync_if_missing`.
pub fn stack_resolver(
    node_stack: &NodeStack,
) -> impl Fn(&str, &str) -> Option<config::node::NodeConfig> + '_ {
    move |name, tag| {
        node_stack
            .find(name, tag)
            .map(|e| e.read().config().clone())
    }
}

#[cfg(test)]
mod implements_tests {
    //! Exercises [`resolve_implements`]: the cache-loading, entry-driven
    //! resolution and Tier B coverage side of `manifest.implements`. The
    //! generator side (module nesting / wire-segment embedding) is verified
    //! by the integration tests in
    //! `crates/generator-internal/tests/{rust,python}/`.

    use super::*;
    use config::node::{Interfaces, Manifest};
    use core_node_api::encoding::RepoSourceKind;
    use generator::InterfaceVariant;
    use std::fs;
    use tempfile::TempDir;

    /// Writes a contract manifest to `dir/{name}_{tag}.json5` and seeds the
    /// returned `ContractCacheEntry` with the matching sha256. Returns
    /// `(entry, abs_path)` so callers can either keep or mutate the entry
    /// (e.g. for the drift test).
    fn seed_contract(
        dir: &std::path::Path,
        name: &str,
        tag: &str,
        body: &str,
    ) -> repo_cache::ContractCacheEntry {
        let file_name = format!("{name}_{tag}.json5");
        let path = dir.join(&file_name);
        fs::write(&path, body).expect("write contract file");
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        repo_cache::ContractCacheEntry {
            contract_name: name.to_string(),
            tag: tag.to_string(),
            sha256: sha,
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            path: path.to_string_lossy().to_string(),
            repo_id: 0,
        }
    }

    /// Builds a contracts.json5 cache + dir-rooted `PeppyDirs` from a set of
    /// seeded entries.
    fn make_peppy_dirs_with_cache(
        entries: &[repo_cache::ContractCacheEntry],
    ) -> (TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let dirs = PeppyDirs::new(tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        let json = serde_json5::to_string(&entries.to_vec()).expect("serialize cache");
        fs::write(&cache_path, json).expect("write cache file");
        (tmp, dirs)
    }

    const DEPTH_V1_BODY: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "depth_camera", tag: "v1" },
        interfaces: {
            topics: [
                { name: "video_stream", qos_profile: "sensor_data" }
            ]
        }
    }"#;

    fn manifest_with_implements(implements: &str) -> Manifest {
        serde_json5::from_str(&format!(
            r#"{{ name: "camera_node", tag: "v1", implements: [{implements}] }}"#
        ))
        .expect("manifest parses")
    }

    fn interfaces_from(json5: &str) -> Interfaces {
        serde_json5::from_str(json5).expect("interfaces parses")
    }

    #[test]
    fn returns_empty_when_no_implements() {
        let dirs = PeppyDirs::new(TempDir::new().unwrap().path().to_path_buf());
        let manifest: Manifest = serde_json5::from_str(r#"{ name: "plain", tag: "v1" }"#).unwrap();
        let out =
            resolve_implements(&manifest, &Interfaces::default(), &dirs, &|_| {}).expect("ok");
        assert!(out.is_empty());
    }

    /// Happy path: a full-coverage manifest yields one DeploymentInterface
    /// per contract-backed entry, each carrying the member's shape from the
    /// contract document and a `ContractOrigin` pointing back at the source
    /// contract.
    #[test]
    fn resolves_full_coverage_with_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let out = resolve_implements(&manifest, &interfaces, &dirs, &|_| {}).expect("happy path");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(
                    topic.qos_profile,
                    config::node::QoSProfile::SensorData,
                    "shape and qos come from the contract document"
                );
                assert_eq!(o.contract_name, "depth_camera");
                assert_eq!(o.contract_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    #[test]
    fn partial_coverage_rejected_with_aggregated_diff() {
        const UVC_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
            interfaces: {
                topics: [ { name: "video_stream" } ],
                services: [ { name: "video_stream_info" }, { name: "set_contrast" } ]
            }
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "uvc_camera", "v1", UVC_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#);
        // Missing both services; carries one unknown extra.
        let interfaces = interfaces_from(
            r#"{
                topics: { emits: [
                    { link_id: "cam", name: "video_stream" },
                    { link_id: "cam", name: "not_in_contract" },
                ] },
            }"#,
        );

        let err = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect_err("partial coverage must error");
        let ImplementsError::Coverage(mismatches) = &err else {
            panic!("coverage failure must carry structured mismatches, got {err:?}");
        };
        assert_eq!(mismatches.len(), 1, "one broken slot, one mismatch");
        let err = err.to_string();
        assert!(
            err.contains("uvc_camera:v1") && err.contains("cam"),
            "error should name the contract and slot, got: {err}"
        );
        assert!(
            err.contains("video_stream_info") && err.contains("set_contrast"),
            "error should list every missing member at once, got: {err}"
        );
        assert!(
            err.contains("not_in_contract"),
            "error should list unknown names, got: {err}"
        );
    }

    #[test]
    fn wrong_kind_entry_reported_in_diff() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        // The contract's one topic listed under services.exposes.
        let interfaces = interfaces_from(
            r#"{ services: { exposes: [{ link_id: "cam", name: "video_stream" }] } }"#,
        );

        let err = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect_err("wrong-kind entry must error")
            .to_string();
        assert!(
            err.contains("video_stream") && err.contains("wrong kind"),
            "error should flag the wrong-kind entry, got: {err}"
        );
        assert!(
            err.contains("declared as service, contract declares topic"),
            "error should say which kinds are involved, got: {err}"
        );
    }

    /// One complete slot + one broken slot: the error names only the broken
    /// one.
    #[test]
    fn multi_slot_error_names_only_the_broken_slot() {
        const IMU_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "imu", tag: "v1" },
            interfaces: { topics: [ { name: "orientation" } ] }
        }"#;
        let tmp = TempDir::new().unwrap();
        let depth = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let imu = seed_contract(tmp.path(), "imu", "v1", IMU_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[depth, imu]);

        let manifest = manifest_with_implements(
            r#"{ name: "depth_camera", tag: "v1", link_id: "cam" },
               { name: "imu", tag: "v1", link_id: "motion" }"#,
        );
        // cam fully covered; motion missing its one topic.
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let err = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect_err("broken slot must error")
            .to_string();
        assert!(
            err.contains("imu:v1") && err.contains("motion") && err.contains("orientation"),
            "error should name the broken slot and its missing member, got: {err}"
        );
        assert!(
            !err.contains("depth_camera:v1"),
            "the complete slot must not be flagged, got: {err}"
        );
    }

    /// A zero-member contract with zero entries is degenerately
    /// coverage-complete.
    #[test]
    fn zero_member_contract_with_zero_entries_passes() {
        const EMPTY_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "empty_contract", tag: "v1" },
            interfaces: {}
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "empty_contract", "v1", EMPTY_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "empty_contract", tag: "v1", link_id: "noop" }"#);
        let out = resolve_implements(&manifest, &Interfaces::default(), &dirs, &|_| {})
            .expect("degenerate coverage passes");
        assert!(out.is_empty());
    }

    #[test]
    fn cache_miss_suggests_repo_refresh() {
        // Empty cache; any lookup misses.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);
        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);

        let err = resolve_implements(&manifest, &Interfaces::default(), &dirs, &|_| {})
            .expect_err("miss must error")
            .to_string();
        assert!(
            err.contains("`depth_camera:v1`") && err.contains("peppy repo refresh"),
            "missing-from-cache error should name the entry and suggest refresh, got: {err}"
        );
    }

    /// Services and actions resolve through the same entry-driven path with
    /// origins stamped, including the (not-yet-used-in-production)
    /// contract-declared action shape.
    #[test]
    fn resolves_service_and_action_members_with_origin() {
        const ARM_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                services: [ { name: "control" } ],
                actions: [ { name: "move_arm" } ]
            }
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm", "v1", ARM_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest = manifest_with_implements(r#"{ name: "arm", tag: "v1", link_id: "arm" }"#);
        let interfaces = interfaces_from(
            r#"{
                services: { exposes: [{ link_id: "arm", name: "control" }] },
                actions: { exposes: [{ link_id: "arm", name: "move_arm" }] },
            }"#,
        );

        let out = resolve_implements(&manifest, &interfaces, &dirs, &|_| {}).expect("happy path");

        let mut saw_service = false;
        let mut saw_action = false;
        for entry in &out {
            match entry.interface() {
                InterfaceVariant::ExposedService {
                    service,
                    origin: Some(o),
                } => {
                    assert_eq!(service.name, "control");
                    assert_eq!(o.contract_name, "arm");
                    assert_eq!(o.contract_tag, "v1");
                    saw_service = true;
                }
                InterfaceVariant::ExposedAction {
                    action,
                    origin: Some(o),
                } => {
                    assert_eq!(action.name, "move_arm");
                    assert_eq!(o.contract_name, "arm");
                    assert_eq!(o.contract_tag, "v1");
                    saw_action = true;
                }
                other => panic!("unexpected resolved variant: {other:?}"),
            }
        }
        assert!(saw_service, "service should be resolved with origin");
        assert!(saw_action, "action should be resolved with origin");
    }

    #[test]
    fn sha256_drift_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        // Rewrite the underlying file so its fingerprint no longer matches
        // the cache's `sha256`, i.e. the cache thinks the file is X but it
        // is now Y. resolve_implements must catch this.
        fs::write(
            &entry.path,
            DEPTH_V1_BODY.replace("video_stream", "video_stream_v2"),
        )
        .unwrap();
        // Keep the stale (pre-rewrite) sha256 in the cache entry. We need to
        // ensure load_contract_cache trusts it.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);
        let err = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect_err("drift must error")
            .to_string();
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }

    /// Seeds a local git repository at `repo_dir` with a single file at the
    /// given repo-relative path, then returns the branch name (e.g. "main"
    /// or "master") that `ensure_checkout` can target.
    fn init_git_repo_with_contract(
        repo_dir: &std::path::Path,
        repo_relative_path: &str,
        body: &str,
    ) -> String {
        let repo = git2::Repository::init(repo_dir).expect("git init");
        let abs = repo_dir.join(repo_relative_path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&abs, body).expect("write contract file");

        let mut index = repo.index().expect("open index");
        index
            .add_path(std::path::Path::new(repo_relative_path))
            .expect("add path");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("Peppy", "peppy@example.com").expect("signature");
        repo.commit(Some("HEAD"), &sig, &sig, "seed contract", &tree, &[])
            .expect("commit");
        repo.head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned()
    }

    /// A `manifest.implements` slot whose cache record is git-sourced (so
    /// `entry.path` is repo-relative) must materialize the checkout via
    /// `ensure_checkout` and read from the joined absolute path, not from
    /// CWD.
    #[test]
    fn resolve_implements_git_sourced_contract_reads_from_checkout() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("contracts_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let branch = init_git_repo_with_contract(
            &source_repo_dir,
            "cameras/depth_camera.json5",
            DEPTH_V1_BODY,
        );
        let repo_url = source_repo_dir.display().to_string();

        let entry = repo_cache::ContractCacheEntry {
            contract_name: "depth_camera".to_string(),
            tag: "v1".to_string(),
            sha256: config::fingerprint::fingerprint_for_bytes(DEPTH_V1_BODY.as_bytes()),
            source_type: RepoSourceKind::Git,
            source_uri: Some(repo_url),
            resolved_ref: Some(branch),
            path: "cameras/depth_camera.json5".to_string(),
            repo_id: 0,
        };
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        fs::write(
            &cache_path,
            serde_json5::to_string(&vec![entry]).expect("serialize cache"),
        )
        .expect("write cache file");

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let out = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect("git-sourced implements should resolve");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(o.contract_name, "depth_camera");
                assert_eq!(o.contract_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    /// Companion to the git happy-path test: proves the drift fingerprint
    /// check runs against the *resolved* (checkout-joined) path, not just
    /// the cache record. Pinning a stale sha256 in the cache must still
    /// trigger the drift error even when the on-disk content lives in a
    /// git checkout the resolver has to materialize first.
    #[test]
    fn resolve_implements_git_sourced_drift_detected() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("contracts_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let branch = init_git_repo_with_contract(
            &source_repo_dir,
            "cameras/depth_camera.json5",
            DEPTH_V1_BODY,
        );
        let repo_url = source_repo_dir.display().to_string();

        let entry = repo_cache::ContractCacheEntry {
            contract_name: "depth_camera".to_string(),
            tag: "v1".to_string(),
            // Deliberately wrong fingerprint; must trigger drift detection.
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            source_type: RepoSourceKind::Git,
            source_uri: Some(repo_url),
            resolved_ref: Some(branch),
            path: "cameras/depth_camera.json5".to_string(),
            repo_id: 0,
        };
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        fs::write(
            &cache_path,
            serde_json5::to_string(&vec![entry]).expect("serialize cache"),
        )
        .expect("write cache file");

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let err = resolve_implements(&manifest, &interfaces, &dirs, &|_| {})
            .expect_err("git-sourced drift must error")
            .to_string();
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }

    #[test]
    fn consumed_service_with_response_only_format_resolves() {
        const UVC_V1_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
            interfaces: {
                services: [
                    {
                        name: "video_stream_info",
                        response_message_format: {
                            width: "u32",
                            height: "u32",
                            frames_per_second: "u8",
                        }
                    }
                ]
            }
        }"#;

        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "uvc_camera", "v1", UVC_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "uvc_consumer",
                tag: "v1",
                depends_on: {
                    contracts: [
                        { name: "uvc_camera", tag: "v1", link_id: "camera" }
                    ]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg: Interfaces = serde_json5::from_str(
            r#"{
                services: {
                    consumes: [
                        { link_id: "camera", name: "video_stream_info" }
                    ]
                }
            }"#,
        )
        .expect("interfaces parses");

        let out = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, &|_| {})
            .expect("response-only service must resolve");

        assert_eq!(
            out.len(),
            1,
            "expected exactly one ConsumedService, got {} entries (pre-fix this was 0: \
             the service was silently dropped)",
            out.len()
        );
        match out[0].interface() {
            InterfaceVariant::ConsumedService {
                service,
                request_format,
                response_format,
                ..
            } => {
                assert_eq!(service.name, "video_stream_info");
                assert!(
                    request_format.0.is_empty(),
                    "response-only service should have empty request format"
                );
                assert_eq!(
                    response_format.0.len(),
                    3,
                    "response format should preserve all three declared fields"
                );
            }
            other => panic!("expected ConsumedService variant, got {other:?}"),
        }
    }

    /// Node-dependency consumption resolves the producer's NATIVE entries
    /// only; the same interface consumed via a contract dep is
    /// contract-addressed.
    #[test]
    fn node_dep_consumption_is_native_only() {
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);

        let producer: config::node::NodeConfig = config::node::NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "hybrid", tag: "v1",
                    implements: [{ name: "depth_camera", tag: "v1", link_id: "cam" }],
                },
                interfaces: {
                    topics: { emits: [
                        { link_id: "cam", name: "video_stream" },
                        { name: "debug_stream", message_format: { x: "f64" } },
                    ] },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        )
        .expect("producer parses");

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "consumer",
                tag: "v1",
                depends_on: {
                    nodes: [ { name: "hybrid", tag: "v1", link_id: "producer" } ]
                }
            }"#,
        )
        .expect("manifest parses");

        // Native name resolves, node-addressed.
        let native_cfg: Interfaces = serde_json5::from_str(
            r#"{ topics: { consumes: [{ link_id: "producer", name: "debug_stream" }] } }"#,
        )
        .unwrap();
        let out = collect_consumed_interfaces(
            &manifest,
            &native_cfg,
            |name, _| (name == "hybrid").then(|| producer.clone()),
            &dirs,
            &|_| {},
        )
        .expect("native consumption resolves");
        assert_eq!(out.len(), 1);
        match out[0].interface() {
            InterfaceVariant::ConsumedTopic { dependency, .. } => {
                assert!(
                    dependency.origin.is_none(),
                    "node-dep consumption must be node-addressed"
                );
            }
            other => panic!("expected ConsumedTopic, got {other:?}"),
        }

        // Contract-backed-only name does not resolve through the node dep
        // (validate_dependency_specs reports the dedicated error upstream;
        // resolution simply finds no native offering here).
        let contract_backed_cfg: Interfaces = serde_json5::from_str(
            r#"{ topics: { consumes: [{ link_id: "producer", name: "video_stream" }] } }"#,
        )
        .unwrap();
        let out = collect_consumed_interfaces(
            &manifest,
            &contract_backed_cfg,
            |name, _| (name == "hybrid").then(|| producer.clone()),
            &dirs,
            &|_| {},
        )
        .expect("collection itself does not fail");
        assert!(
            out.is_empty(),
            "a contract-backed-only name must not resolve through a node dep"
        );
    }
}
