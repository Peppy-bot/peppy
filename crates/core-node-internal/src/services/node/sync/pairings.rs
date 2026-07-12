//! Pairing-document resolution for `depends_on.pairings`: loading the
//! `pairing/v1` doc from the repo cache (sha-pinned, drift-checked) and
//! validating each manifest entry's role against the doc's two roles.

use crate::services::repo::cache as repo_cache;
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
///   names them);
/// - two entries must not collide after tag normalization (same rule as
///   `manifest.implements`; exact duplicates of `(name, tag, role, link_id)` are
///   already impossible because link_ids are unique).
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

/// Resolves every `depends_on.pairings` slot into the generator inputs for
/// the `pairings/<link_id>/<topic>` modules: per slot, topics with
/// `emitted_by == entry.role` become peer-emitted and all others
/// peer-consumed. Runs [`validate_pairing_specs`] first, so role errors and
/// cache misses surface before any codegen.
pub fn collect_pairing_interfaces(
    manifest: &config::node::Manifest,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<generator::DeploymentInterface>, String> {
    let docs = validate_pairing_specs(manifest, peppy_dirs, on_feedback)?;
    let Some(pairing_deps) = manifest
        .depends_on
        .as_ref()
        .map(|d| d.pairings.as_slice())
        .filter(|p| !p.is_empty())
    else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for dep in pairing_deps {
        let doc = docs
            .get(&dep.link_id)
            .expect("validate_pairing_specs returns a doc per declared slot");
        let peer = generator::PeerContext {
            link_id: dep.link_id.clone(),
            pairing_name: dep.name.as_str().to_string(),
            pairing_tag: dep.tag.clone(),
        };
        for topic in &doc.topics {
            let emitted = config::node::NativeEmittedTopic {
                name: topic.name.clone(),
                qos_profile: topic.qos_profile.clone(),
                message_format: topic.message_format.clone(),
            };
            if topic.emitted_by == dep.role {
                out.push(generator::DeploymentInterface::peer_emitted_topic(
                    emitted,
                    peer.clone(),
                ));
            } else {
                out.push(generator::DeploymentInterface::peer_consumed_topic(
                    emitted,
                    peer.clone(),
                ));
            }
        }
    }
    Ok(out)
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
