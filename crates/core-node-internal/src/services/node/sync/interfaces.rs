use super::deps::{
    DependencyKind, DependencyLookupEntry, DependencyOfferings,
    build_dependency_context_for_interface, build_dependency_context_for_node,
    build_dependency_lookup, build_dependency_offerings,
};
use crate::services::repo::cache as repo_cache;
use daemon_config::consts::PeppyDirs;
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceOrigin};
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
/// interfaces from `depends_on`, `conforms_to` documents, and both
/// directions of every declared pairing slot (role-validated, sha-pinned,
/// drift-checked). Used by `node add`, `node sync`, and the launch-time
/// auto-sync so the "what feeds peppygen" sequence lives in one place.
/// Error strings name the failing step; callers only add their transport
/// wrapper.
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
        resolve_conforms_to(interfaces_cfg, peppy_dirs, on_feedback)
            .map_err(|reason| format!("failed to resolve `conforms_to` interfaces: {reason}"))?,
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
    // that merges native emits/exposes (origin `None`) with conformed entries
    // (origin `Some(_)`). Native wins on key collision so the consumer side
    // still addresses native producers via `SenderTarget::Node` and conformed
    // ones via `SenderTarget::Interface`.
    let mut node_dep_offerings: HashMap<(String, String), DependencyOfferings> = HashMap::new();
    // Memoized parsed contracts for `depends_on.contracts`
    // entries, keyed by `link_id` so two entries with the same
    // `(name, tag)` but different sha256 pins are cached and resolved
    // separately. `resolve_contract_doc` handles SHA-pin matching and
    // on-disk drift detection per load.
    let mut iface_dep_contracts: HashMap<String, daemon_config::contract::PeppyContract> =
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
                let conformed =
                    resolve_conforms_to(&dep_config.interfaces, peppy_dirs, on_feedback).map_err(
                        |e| {
                            format!(
                                "failed to resolve `conforms_to` for dependency `{}:{}`: {e}",
                                entry.name, entry.tag
                            )
                        },
                    )?;
                node_dep_offerings.insert(
                    node_key,
                    build_dependency_offerings(&dep_config, &conformed),
                );
            }
            DependencyKind::Interface => {
                if iface_dep_contracts.contains_key(link_id) {
                    continue;
                }
                let parsed = resolve_contract_doc(
                    peppy_dirs,
                    &entry.name,
                    &entry.tag,
                    entry.sha256.as_deref(),
                    on_feedback,
                )?;
                iface_dep_contracts.insert(link_id.clone(), parsed);
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
                &iface_dep_contracts,
                &consumed_topic.link_id,
                consumed_topic.name.trim(),
                |offerings, name| {
                    offerings
                        .topics
                        .get(name)
                        .map(|(mf, origin)| (mf.clone(), origin.clone()))
                },
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
                &iface_dep_contracts,
                &consumed_service.link_id,
                consumed_service.name.trim(),
                |offerings, name| {
                    offerings
                        .services
                        .get(name)
                        .map(|(req, resp, origin)| ((req.clone(), resp.clone()), origin.clone()))
                },
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
                &iface_dep_contracts,
                &consumed_action.link_id,
                consumed_action.name.trim(),
                |offerings, name| {
                    offerings
                        .actions
                        .get(name)
                        .map(|(msg, origin)| (msg.clone(), origin.clone()))
                },
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
/// `DependencyContext` the generator needs to address it. Walks both node and
/// interface backings via the caller-supplied extractors; for nodes the
/// extractor must surface the `Option<InterfaceOrigin>` from the offering so
/// the consumer reaches `conforms_to` producers via `SenderTarget::Interface`.
fn resolve_consumed_offering<T>(
    dep_lookup: &HashMap<String, DependencyLookupEntry>,
    node_dep_offerings: &HashMap<(String, String), DependencyOfferings>,
    iface_dep_contracts: &HashMap<String, daemon_config::contract::PeppyContract>,
    link_id: &str,
    lookup_name: &str,
    extract_from_node: impl FnOnce(
        &DependencyOfferings,
        &str,
    ) -> Option<(T, Option<generator::InterfaceOrigin>)>,
    extract_from_interface: impl FnOnce(&daemon_config::contract::PeppyContract, &str) -> Option<T>,
) -> Option<(T, generator::DependencyContext)> {
    let entry = dep_lookup.get(link_id)?;
    match entry.kind {
        DependencyKind::Node => {
            let offerings = node_dep_offerings.get(&(entry.name.clone(), entry.tag.clone()))?;
            let (extracted, origin) = extract_from_node(offerings, lookup_name)?;
            Some((
                extracted,
                build_dependency_context_for_node(&entry.name, &entry.tag, origin, link_id),
            ))
        }
        DependencyKind::Interface => {
            let parsed = iface_dep_contracts.get(link_id)?;
            let extracted = extract_from_interface(parsed, lookup_name)?;
            Some((
                extracted,
                build_dependency_context_for_interface(&entry.name, &entry.tag, link_id),
            ))
        }
    }
}

pub(super) fn action_message_from_exposed(
    exposed_action: &config::node::ExposedAction,
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
/// [`resolve_conforms_to`] (producer side) and the `depends_on.contracts`
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

/// Resolves every `interfaces.conforms_to` entry against the local interface
/// cache and returns the pulled interface's topics/services/actions as a
/// `Vec<DeploymentInterface>` ready to feed [`generator::generate_peppygen_lib`].
///
/// Each returned `DeploymentInterface` is stamped with an
/// [`InterfaceOrigin`] so the generator nests it under
/// `emitted_topics/{iface_name}/{iface_tag}/{leaf}` (and similar for services
/// and actions) and embeds the matching `(iface_name, iface_tag)` segments in
/// the generated wire-path calls.
///
/// Errors:
/// - Duplicate raw `(name, tag)` entries (sha256 differences do not count).
/// - Two entries that sanitize to the same `(iface_name, iface_tag)`, e.g.
///   `v1` and `v-1` collide because the wire-path tag normalization replaces
///   hyphens with underscores. Refusing this keeps generated symbols
///   addressable without ambiguity.
/// - Cache miss, which surfaces "run `peppy repo refresh`".
/// - `sha256` pin set but the on-disk content has drifted.
pub fn resolve_conforms_to(
    interfaces_cfg: &config::node::Interfaces,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let Some(items) = interfaces_cfg.conforms_to.as_ref() else {
        return Ok(Vec::new());
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // Sanitized-key collisions strictly dominate raw-key duplicates (an exact
    // raw dup collides post-sanitize too), so one pass catches both. Compare
    // the prior raw tag to distinguish "duplicate" from "collides after
    // hyphen→underscore normalization" (e.g. `v1` vs `v-1`); both would
    // generate to the same module path and wire segments.
    let mut seen: HashMap<(String, String), String> = HashMap::new();
    for item in items {
        let sanitized_tag = item.tag.replace('-', "_");
        let key = (item.name.as_str().to_string(), sanitized_tag);
        if let Some(prior_tag) = seen.insert(key, item.tag.clone()) {
            if prior_tag == item.tag {
                return Err(format!(
                    "duplicate `conforms_to` entry `{}:{}`",
                    item.name.as_str(),
                    item.tag
                ));
            }
            return Err(format!(
                "`conforms_to` entries `{}:{}` and `{}:{}` collide after \
                 tag normalization (hyphens become underscores); rename one \
                 to disambiguate",
                item.name.as_str(),
                prior_tag,
                item.name.as_str(),
                item.tag
            ));
        }
    }

    let mut out: Vec<DeploymentInterface> = Vec::new();
    for item in items {
        let name = item.name.as_str();
        let tag = item.tag.as_str();
        let parsed =
            resolve_contract_doc(peppy_dirs, name, tag, item.sha256.as_deref(), on_feedback)?;

        let origin = InterfaceOrigin {
            iface_name: name.to_string(),
            iface_tag: tag.to_string(),
        };

        for topic in parsed.interfaces.topics {
            out.push(DeploymentInterface::emitted_topic(
                topic,
                Some(origin.clone()),
            ));
        }
        for service in parsed.interfaces.services {
            out.push(DeploymentInterface::exposed_service(
                service,
                Some(origin.clone()),
            ));
        }
        for action in parsed.interfaces.actions {
            out.push(DeploymentInterface::exposed_action(
                action,
                Some(origin.clone()),
            ));
        }
    }

    Ok(out)
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
mod conforms_to_tests {
    //! Exercises [`resolve_conforms_to`]: the cache-loading side of
    //! `interfaces.conforms_to` resolution. The generator-side (module
    //! nesting / wire-segment embedding) is verified by the integration
    //! tests in `crates/generator-internal/tests/{rust,python}/conforms_to.rs`.

    use super::*;
    use config::node::{ConformsToItem, Interfaces};
    use config::runtime::Name;
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

    fn interfaces_with_conforms(items: Vec<ConformsToItem>) -> Interfaces {
        Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: Some(items),
        }
    }

    #[test]
    fn returns_empty_when_no_conforms_to() {
        let dirs = PeppyDirs::new(TempDir::new().unwrap().path().to_path_buf());
        let cfg = Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: None,
        };
        let out = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect("ok");
        assert!(out.is_empty());
    }

    /// Happy path: a `conforms_to` entry whose `(name, tag)` is present in the
    /// contracts cache yields the underlying contract's topics, each wrapped
    /// as `EmittedTopic` and stamped with `origin` pointing back to the source
    /// contract so downstream codegen can attribute the topic.
    #[test]
    fn resolves_cache_hit_with_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let out = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect("happy path");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(o.iface_name, "depth_camera");
                assert_eq!(o.iface_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    #[test]
    fn cache_miss_suggests_repo_refresh() {
        // Empty cache; any lookup misses.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);
        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let err = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect_err("miss must error");
        assert!(
            err.contains("`depth_camera:v1`") && err.contains("peppy repo refresh"),
            "missing-from-cache error should name the entry and suggest refresh, got: {err}"
        );
    }

    #[test]
    fn duplicate_raw_entries_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        // Two entries with the same raw `(name, tag)`; sha256 differing
        // should NOT rescue this case per the spec.
        let cfg = interfaces_with_conforms(vec![
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v1".to_string(),
                sha256: None,
            },
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v1".to_string(),
                sha256: Some("aaa".to_string()),
            },
        ]);

        let err = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect_err("dup must error");
        assert!(
            err.contains("duplicate") && err.contains("depth_camera:v1"),
            "duplicate error should name the entry, got: {err}"
        );
    }

    #[test]
    fn tag_sanitize_collisions_are_rejected() {
        // `v_1` and `v-1` both sanitize to `v_1` after the hyphen→underscore
        // pass that the wire-path and generated-symbol layers apply. Refuse
        // rather than silently merge.
        let tmp = TempDir::new().unwrap();
        let entry_a = seed_contract(tmp.path(), "depth_camera", "v_1", DEPTH_V1_BODY);
        let body_b = DEPTH_V1_BODY.replace("\"v1\"", "\"v-1\"");
        let entry_b = seed_contract(tmp.path(), "depth_camera", "v-1", &body_b);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry_a, entry_b]);

        let cfg = interfaces_with_conforms(vec![
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v_1".to_string(),
                sha256: None,
            },
            ConformsToItem {
                name: Name::new("depth_camera").unwrap(),
                tag: "v-1".to_string(),
                sha256: None,
            },
        ]);

        let err = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect_err("collision must error");
        assert!(
            err.contains("collide") && err.contains("normalization"),
            "sanitize-collision error should mention collision + normalization, got: {err}"
        );
    }

    const ARM_V1_WITH_SERVICE_AND_ACTION: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "arm", tag: "v1" },
        interfaces: {
            services: [
                { name: "control" }
            ],
            actions: [
                { name: "move_arm" }
            ]
        }
    }"#;

    /// A `conforms_to` entry whose body declares a service AND an action must
    /// yield both as `ExposedService`/`ExposedAction` variants stamped with
    /// `Some(origin)` pointing back at the source interface. Mirrors
    /// `resolves_cache_hit_with_origin` but exercises the non-topic variants.
    #[test]
    fn resolves_cache_hit_with_service_and_action_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm", "v1", ARM_V1_WITH_SERVICE_AND_ACTION);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("arm").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let out = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect("happy path");

        let mut saw_service = false;
        let mut saw_action = false;
        for entry in &out {
            match entry.interface() {
                InterfaceVariant::ExposedService {
                    service,
                    origin: Some(o),
                } => {
                    assert_eq!(service.name, "control");
                    assert_eq!(o.iface_name, "arm");
                    assert_eq!(o.iface_tag, "v1");
                    saw_service = true;
                }
                InterfaceVariant::ExposedAction {
                    action,
                    origin: Some(o),
                } => {
                    assert_eq!(action.name, "move_arm");
                    assert_eq!(o.iface_name, "arm");
                    assert_eq!(o.iface_tag, "v1");
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
        // is now Y. resolve_conforms_to must catch this.
        fs::write(
            &entry.path,
            DEPTH_V1_BODY.replace("video_stream", "video_stream_v2"),
        )
        .unwrap();
        // Keep the stale (pre-rewrite) sha256 in the cache entry. We need to
        // ensure load_contract_cache trusts it.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);
        let err = resolve_conforms_to(&cfg, &dirs, &|_| {}).expect_err("drift must error");
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }

    /// Seeds a local git repository at `repo_dir` with a single file at the
    /// given repo-relative path, then returns the branch name (e.g. "main"
    /// or "master") that `ensure_checkout` can target.
    fn init_git_repo_with_interface(
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

    /// Direct regression for the user-reported bug: a `conforms_to` entry
    /// whose cache record is git-sourced (so `entry.path` is repo-relative)
    /// must materialize the checkout via `ensure_checkout` and read from the
    /// joined absolute path, not from CWD.
    #[test]
    fn resolve_conforms_to_git_sourced_interface_reads_from_checkout() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("interfaces_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let branch = init_git_repo_with_interface(
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

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let out = resolve_conforms_to(&cfg, &dirs, &|_| {})
            .expect("git-sourced conforms_to should resolve");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(o.iface_name, "depth_camera");
                assert_eq!(o.iface_tag, "v1");
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
    fn resolve_conforms_to_git_sourced_drift_detected() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("interfaces_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let branch = init_git_repo_with_interface(
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

        let cfg = interfaces_with_conforms(vec![ConformsToItem {
            name: Name::new("depth_camera").unwrap(),
            tag: "v1".to_string(),
            sha256: None,
        }]);

        let err =
            resolve_conforms_to(&cfg, &dirs, &|_| {}).expect_err("git-sourced drift must error");
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

        let manifest: config::node::Manifest = serde_json5::from_str(
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
}
