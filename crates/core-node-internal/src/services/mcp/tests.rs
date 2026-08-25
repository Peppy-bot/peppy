//! The daemon-side derivations against seeded caches: what the launch
//! resolves from a launcher's exposure list, what a peer materializes from
//! the pins it receives, and what the hub check reports.

use super::*;
use crate::services::repo::cache::{self as repo_cache, ContractCacheEntry, McpExposureCacheEntry};
use crate::services::repo::index::publish_repository_index;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{EntryOrigin, ItemName, ItemTag, ManifestFingerprint, PinKind};
use daemon_config::source::ExposureRef;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const CAMERA_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "rgb_camera", tag: "v1" },
    interfaces: {
        services: [
            {
                name: "video_stream_info",
                response_message_format: { width: "u32", height: "u32" },
            },
        ],
    },
}"#;

/// The same identity at other bytes, for the two-contents refusal.
const CAMERA_CONTRACT_REVISED: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "rgb_camera", tag: "v1" },
    interfaces: {
        services: [
            {
                name: "video_stream_info",
                response_message_format: { width: "u32", height: "u32", fps: "u8" },
            },
        ],
    },
}"#;

const RECORDING_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "episode_recording", tag: "v1" },
    interfaces: {
        actions: [
            {
                name: "record_episode",
                goal_service: { request_message_format: { episode_name: "string" } },
                result_service: { response_message_format: { frames: "u32" } },
            },
        ],
    },
}"#;

fn exposure(
    name: &str,
    target: &str,
    contract: &str,
    sha256: Option<&str>,
    member: &str,
) -> String {
    let pin = sha256
        .map(|sha| format!(", sha256: \"{sha}\""))
        .unwrap_or_default();
    let selection = if contract == "episode_recording" {
        format!(
            r#"actions: [{{ member: "{member}", tool: "{target}.{member}", description: "R.",
                operation: "long_running", deadline_ms: 60000 }}]"#
        )
    } else {
        format!(
            r#"services: [{{ member: "{member}", tool: "{target}.{member}", description: "I.",
                operation: "read_only", deadline_ms: 2000 }}]"#
        )
    };
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "{name}", tag: "v1" }},
        server: {{ title: "{name}" }},
        targets: {{
            {target}: {{
                contract: {{ name: "{contract}", tag: "v1"{pin} }},
                {selection}
            }},
        }},
    }}"#
    )
}

/// A Peppy home whose contract and exposure caches serve documents from
/// files under it: the state a `peppy repo refresh` of one fs repository
/// leaves behind.
struct SeededHome {
    _guard: TempDir,
    dirs: PeppyDirs,
    docs: PathBuf,
}

impl SeededHome {
    fn new(contracts: &[&str], exposures: &[(&str, &str)]) -> Self {
        let guard = TempDir::new().expect("temp home");
        let dirs = PeppyDirs::new(guard.path());
        let docs = guard.path().join("docs");
        std::fs::create_dir_all(&docs).expect("docs dir");
        let contract_entries: Vec<ContractCacheEntry> = contracts
            .iter()
            .enumerate()
            .map(|(index, content)| {
                let document = daemon_config::contract::PeppyContractParser::from_content(content)
                    .expect("contract parses");
                let path = docs.join(format!("contract_{index}.json5"));
                std::fs::write(&path, content).expect("write contract");
                ContractCacheEntry {
                    contract_name: ItemName::parse(document.manifest.name.as_str()).unwrap(),
                    tag: ItemTag::parse(&document.manifest.tag).unwrap(),
                    sha256: ManifestFingerprint::of_bytes(content.as_bytes()),
                    origin: EntryOrigin::Fs { path },
                    repo_id: 0,
                }
            })
            .collect();
        repo_cache::write_repo_cache(&dirs, &contract_entries).expect("write contract cache");
        let exposure_entries: Vec<McpExposureCacheEntry> = exposures
            .iter()
            .map(|(name, content)| {
                let path = docs.join(format!("{name}.json5"));
                std::fs::write(&path, content).expect("write exposure");
                McpExposureCacheEntry {
                    exposure_name: ItemName::parse(name).unwrap(),
                    tag: ItemTag::parse("v1").unwrap(),
                    sha256: ManifestFingerprint::of_bytes(content.as_bytes()),
                    origin: EntryOrigin::Fs { path },
                    repo_id: 0,
                }
            })
            .collect();
        repo_cache::write_repo_cache(&dirs, &exposure_entries).expect("write exposure cache");
        Self {
            _guard: guard,
            dirs,
            docs,
        }
    }
}

fn references(names: &[&str]) -> Vec<ExposureRef> {
    names
        .iter()
        .map(|name| ExposureRef {
            name: (*name).to_owned(),
            tag: "v1".to_owned(),
        })
        .collect()
}

fn quiet(_: &str) {}

#[test]
fn a_launch_resolves_exposures_and_their_contracts_through_the_caches() {
    let home = SeededHome::new(
        &[CAMERA_CONTRACT, RECORDING_CONTRACT],
        &[
            (
                "cam",
                &exposure(
                    "cam",
                    "front_camera",
                    "rgb_camera",
                    None,
                    "video_stream_info",
                ),
            ),
            (
                "both",
                &format!(
                    r#"{{
                    peppy_schema: "mcp_exposure/v1",
                    manifest: {{ name: "both", tag: "v1" }},
                    server: {{ title: "Both" }},
                    targets: {{
                        front_camera: {{
                            contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                            services: [{{ member: "video_stream_info", tool: "front_camera.info",
                                description: "I.", operation: "read_only", deadline_ms: 2000 }}],
                        }},
                        recorder: {{
                            contract: {{ name: "episode_recording", tag: "v1" }},
                            actions: [{{ member: "record_episode", tool: "recorder.record",
                                description: "R.", operation: "long_running", deadline_ms: 60000 }}],
                        }},
                    }},
                }}"#,
                    ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes())
                ),
            ),
        ],
    );

    let resolved = resolve_exposure_deployment(&home.dirs, &references(&["both", "cam"]), &quiet)
        .expect("resolves");
    assert_eq!(resolved.plan.name.as_str(), "mcp_both_v1_cam_v1");
    let mut contract_pins: Vec<String> =
        resolved.contract_pins().iter().map(|p| p.label()).collect();
    contract_pins.sort();
    assert_eq!(
        contract_pins,
        [
            "contract `episode_recording:v1`",
            "contract `rgb_camera:v1`"
        ],
        "one pin per contract identity, whichever exposures reference it"
    );
    assert_eq!(
        resolved
            .exposure_pins()
            .iter()
            .map(|pin| pin.kind)
            .collect::<Vec<_>>(),
        [PinKind::McpExposure, PinKind::McpExposure]
    );
    let slots: Vec<&str> = resolved
        .plan
        .config
        .manifest
        .depends_on
        .as_ref()
        .expect("slots")
        .contracts
        .iter()
        .map(|slot| slot.link_id.as_str())
        .collect();
    assert_eq!(slots, ["front_camera", "recorder"]);

    // A peer materializing the same pins plans the same deployment.
    let materialized = materialize_exposure_deployment(
        &home.dirs,
        &resolved.exposure_pins(),
        &resolved.contract_pins(),
        &quiet,
    )
    .expect("materializes");
    assert_eq!(materialized.spec, resolved.spec);
    assert_eq!(
        serde_json::to_value(&materialized.plan.config).unwrap(),
        serde_json::to_value(&resolved.plan.config).unwrap()
    );
}

#[test]
fn two_exposures_pinning_one_contract_at_different_bytes_are_refused_by_name() {
    let first = ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).to_string();
    let second = ManifestFingerprint::of_bytes(CAMERA_CONTRACT_REVISED.as_bytes()).to_string();
    let home = SeededHome::new(
        &[CAMERA_CONTRACT, CAMERA_CONTRACT_REVISED],
        &[
            (
                "a",
                &exposure("a", "cam", "rgb_camera", Some(&first), "video_stream_info"),
            ),
            (
                "b",
                &exposure("b", "cam", "rgb_camera", Some(&second), "video_stream_info"),
            ),
        ],
    );
    let error = resolve_exposure_deployment(&home.dirs, &references(&["a", "b"]), &quiet)
        .expect_err("two contents for one contract");
    assert!(
        error.contains("`a:v1`") && error.contains("`b:v1`"),
        "{error}"
    );
    assert!(error.contains("rgb_camera:v1"), "{error}");
    assert!(error.contains(&first) && error.contains(&second), "{error}");
}

#[test]
fn an_exposure_missing_from_the_cache_is_a_refusal_naming_it() {
    let home = SeededHome::new(&[CAMERA_CONTRACT], &[]);
    let error = resolve_exposure_deployment(&home.dirs, &references(&["absent"]), &quiet)
        .expect_err("nothing cached under that name");
    assert!(error.contains("`absent:v1`"), "{error}");
    assert!(error.contains("peppy repo refresh"), "{error}");
}

#[test]
fn the_catalog_of_a_cached_exposure_derives_from_the_same_plan() {
    let home = SeededHome::new(
        &[CAMERA_CONTRACT],
        &[(
            "cam",
            &exposure(
                "cam",
                "front_camera",
                "rgb_camera",
                None,
                "video_stream_info",
            ),
        )],
    );
    let bundle = derive_exposure_catalog(&home.dirs, "cam", "v1", &quiet).expect("derives");
    assert_eq!(bundle.exposure.endpoint_path(), "/cam/v1/mcp");
    assert_eq!(bundle.tools[0].name, "front_camera.video_stream_info");
    assert_eq!(
        bundle.node.contracts[0].sha256,
        ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).as_str()
    );
}

/// A hub whose index lists the given exposure documents.
fn hub_with(root: &Path, exposures: &[(&str, &str)]) {
    std::fs::create_dir_all(root.join("exposures")).expect("exposures dir");
    for (name, content) in exposures {
        std::fs::write(
            root.join("exposures").join(format!("{name}.json5")),
            content,
        )
        .expect("write exposure");
    }
    publish_repository_index(root).expect("the hub indexes");
}

#[test]
fn the_hub_check_reports_every_problem_of_every_exposure_and_passes_the_valid_ones() {
    let home = SeededHome::new(&[CAMERA_CONTRACT], &[]);
    let hub = home.docs.join("hub");
    hub_with(
        &hub,
        &[
            (
                "good",
                &exposure("good", "cam", "rgb_camera", None, "video_stream_info"),
            ),
            (
                "missing",
                &exposure("missing", "cam", "rgb_camera", None, "no_such"),
            ),
            (
                "unpinned",
                &exposure("unpinned", "cam", "ghost", None, "haunt"),
            ),
            (
                "mispinned",
                &exposure(
                    "mispinned",
                    "cam",
                    "rgb_camera",
                    Some(&"c".repeat(64)),
                    "video_stream_info",
                ),
            ),
        ],
    );
    let findings = check_repository_exposures(&hub, &home.dirs, &quiet).expect("the check runs");
    let ids: Vec<&str> = findings.iter().map(|finding| finding.id.as_str()).collect();
    assert_eq!(ids, ["mispinned:v1", "missing:v1", "unpinned:v1"]);
    let by_id = |id: &str| -> &ExposureFinding {
        findings.iter().find(|finding| finding.id == id).unwrap()
    };
    assert!(by_id("missing:v1").problems[0].contains("declares no such service"));
    assert!(by_id("unpinned:v1").problems[0].contains("contract `ghost:v1`"));
    assert!(by_id("mispinned:v1").problems[0].contains("not in contract cache"));
    assert_eq!(by_id("missing:v1").path, "exposures/missing.json5");
    let rendered = by_id("unpinned:v1").to_string();
    assert!(
        rendered.starts_with("mcp_exposure `unpinned:v1` (exposures/unpinned.json5):"),
        "{rendered}"
    );
}

#[test]
fn the_hub_check_names_a_contract_it_cannot_resolve_rather_than_passing() {
    let empty = TempDir::new().expect("empty home");
    let dirs = PeppyDirs::new(empty.path());
    let hub = empty.path().join("hub");
    hub_with(
        &hub,
        &[(
            "good",
            &exposure("good", "cam", "rgb_camera", None, "video_stream_info"),
        )],
    );
    let findings = check_repository_exposures(&hub, &dirs, &quiet).expect("the check runs");
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].problems[0].contains("contract `rgb_camera:v1` not in contract cache"),
        "{:?}",
        findings[0].problems
    );
}

#[test]
fn an_exposure_file_that_does_not_parse_is_reported_by_path() {
    let home = SeededHome::new(&[CAMERA_CONTRACT], &[]);
    let hub = home.docs.join("hub");
    hub_with(
        &hub,
        &[
            (
                "good",
                &exposure("good", "cam", "rgb_camera", None, "video_stream_info"),
            ),
            (
                "twice",
                r#"{
                peppy_schema: "mcp_exposure/v1",
                manifest: { name: "twice", tag: "v1" },
                server: { title: "Twice" },
                targets: {
                    cam: {
                        contract: { name: "rgb_camera", tag: "v1" },
                        services: [
                            { member: "video_stream_info", tool: "cam.same", description: "A.",
                              operation: "read_only", deadline_ms: 2000 },
                            { member: "video_stream_info", tool: "cam.same", description: "B.",
                              operation: "read_only", deadline_ms: 2000 },
                        ],
                    },
                },
            }"#,
            ),
        ],
    );
    let findings = check_repository_exposures(&hub, &home.dirs, &quiet).expect("the check runs");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].id, "exposures/twice.json5");
    assert!(
        findings[0].problems[0].contains("does not parse as an exposure"),
        "{:?}",
        findings[0].problems
    );
    assert!(
        findings[0].problems[0].contains("more than once"),
        "{:?}",
        findings[0].problems
    );
}

#[test]
fn a_hub_without_exposures_has_nothing_to_report() {
    let empty = TempDir::new().expect("empty home");
    let dirs = PeppyDirs::new(empty.path());
    let hub = empty.path().join("hub");
    hub_with(&hub, &[]);
    assert!(
        check_repository_exposures(&hub, &dirs, &quiet)
            .expect("runs")
            .is_empty()
    );
}
