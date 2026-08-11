//! Exposure publication: resolve an `mcp_exposure/v1` document's pinned
//! contracts through the local repository caches, validate the exposure
//! against them, and publish (or verify) the committed artifacts next to the
//! document: the bundle file and the generated MCP server node.
//!
//! The validation itself is [`generator::build_exposure_bundle`] and the
//! node emission [`generator::generate_exposure_node_from_bundle`], both
//! pure functions;
//! this module is the repository-facing shell that turns sha256 pins into
//! contract bytes and generated values into committed files.

use crate::services::node::resolve_contract_doc;
use daemon_config::consts::PeppyDirs;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use generator::{
    ExposureBundle, GeneratedServerNode, ResolvedContractDocument, build_exposure_bundle,
    generate_exposure_node_from_bundle,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The artifacts published (or verified) for one exposure document.
#[derive(Debug)]
pub struct PublishedExposure {
    pub bundle_path: PathBuf,
    pub bundle: ExposureBundle,
    /// The generated node's directory.
    pub node_dir: PathBuf,
    pub node_file_count: usize,
}

/// One way the committed artifacts differ from what the exposure document
/// produces right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposureDrift {
    BundleMissing { expected_path: String },
    BundleOutdated { path: String },
    NodeFileMissing { expected_path: String },
    NodeFileOutdated { path: String },
}

impl std::fmt::Display for ExposureDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BundleMissing { expected_path } => {
                write!(f, "no bundle is committed at {expected_path}")
            }
            Self::BundleOutdated { path } => {
                write!(
                    f,
                    "{path} does not match the bundle its exposure document produces"
                )
            }
            Self::NodeFileMissing { expected_path } => {
                write!(f, "the generated node is missing {expected_path}")
            }
            Self::NodeFileOutdated { path } => {
                write!(
                    f,
                    "{path} does not match the node its exposure document produces"
                )
            }
        }
    }
}

/// The bundle file published for `exposure_path`: a `.bundle.json` sibling
/// sharing the document's file stem.
pub fn exposure_bundle_path(exposure_path: &Path) -> PathBuf {
    let stem = exposure_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("exposure");
    exposure_path.with_file_name(format!("{stem}.bundle.json"))
}

/// The generated node's directory: a sibling of the exposure document named
/// after the node.
fn exposure_node_dir(exposure_path: &Path, node_name: &str) -> PathBuf {
    exposure_path.with_file_name(node_name)
}

/// Validates the exposure at `exposure_path` against its pinned contracts
/// (resolved through the local repository caches) and publishes the bundle
/// file and the generated MCP server node next to it.
pub fn publish_exposure(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PublishedExposure, String> {
    let rendered = render_artifacts(exposure_path, peppy_dirs, on_feedback)?;
    daemon_config::atomic_write::publish_atomic(&rendered.bundle_path, |tmp| {
        std::fs::write(tmp, &rendered.bundle_content)
    })
    .map_err(|e| format!("failed to write {}: {e}", rendered.bundle_path.display()))?;

    let node_dir = exposure_node_dir(exposure_path, &rendered.node.node_dir_name);
    for file in &rendered.node.files {
        let path = node_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        daemon_config::atomic_write::publish_atomic(&path, |tmp| {
            std::fs::write(tmp, &file.content)
        })
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    }

    Ok(PublishedExposure {
        bundle_path: rendered.bundle_path,
        bundle: rendered.bundle,
        node_dir,
        node_file_count: rendered.node.files.len(),
    })
}

/// Verifies the committed artifacts against what the exposure document
/// produces right now. An empty list means everything matches. Only
/// generated files are compared; extra files a hub commits alongside them
/// (such as a `Cargo.lock`) are not judged here.
pub fn check_exposure(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<ExposureDrift>, String> {
    let rendered = render_artifacts(exposure_path, peppy_dirs, on_feedback)?;
    let mut drifts = Vec::new();

    match std::fs::read_to_string(&rendered.bundle_path) {
        Ok(committed) if committed == rendered.bundle_content => {}
        Ok(_) => drifts.push(ExposureDrift::BundleOutdated {
            path: rendered.bundle_path.display().to_string(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            drifts.push(ExposureDrift::BundleMissing {
                expected_path: rendered.bundle_path.display().to_string(),
            });
        }
        Err(e) => {
            return Err(format!(
                "failed to read {}: {e}",
                rendered.bundle_path.display()
            ));
        }
    }

    let dir = exposure_node_dir(exposure_path, &rendered.node.node_dir_name);
    for file in &rendered.node.files {
        let path = dir.join(&file.path);
        match std::fs::read_to_string(&path) {
            Ok(committed) if committed == file.content => {}
            Ok(_) => drifts.push(ExposureDrift::NodeFileOutdated {
                path: path.display().to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drifts.push(ExposureDrift::NodeFileMissing {
                    expected_path: path.display().to_string(),
                });
            }
            Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
        }
    }

    Ok(drifts)
}

struct RenderedExposure {
    bundle_path: PathBuf,
    bundle: ExposureBundle,
    bundle_content: String,
    node: GeneratedServerNode,
}

/// Parse the exposure, resolve every pinned contract it references, and
/// build the bundle plus the node.
fn render_artifacts(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<RenderedExposure, String> {
    let exposure = PeppyMcpExposureParser::from_path(exposure_path)
        .map_err(|e| format!("{}: {e}", exposure_path.display()))?;

    // Two targets may pin the same reference; each distinct pin is resolved
    // once. Distinct pins on one identity both resolve so the validation
    // can name the collision itself.
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut contracts = Vec::new();
    for target in exposure.targets.values() {
        let reference = &target.contract;
        let key = (
            reference.name.as_str().to_owned(),
            reference.tag.clone(),
            reference.sha256.as_str().to_owned(),
        );
        if !seen.insert(key) {
            continue;
        }
        // Resolution honors the pin: it succeeds only with bytes whose
        // fingerprint equals `reference.sha256`.
        let document = resolve_contract_doc(
            peppy_dirs,
            reference.name.as_str(),
            &reference.tag,
            Some(reference.sha256.as_str()),
            None,
            on_feedback,
        )?;
        contracts.push(ResolvedContractDocument {
            sha256: reference.sha256.clone(),
            document,
        });
    }

    let bundle = build_exposure_bundle(&exposure, &contracts).map_err(|e| e.to_string())?;
    let node = generate_exposure_node_from_bundle(&bundle, &exposure, &contracts);
    let content = bundle.to_json_string();
    Ok(RenderedExposure {
        bundle_path: exposure_bundle_path(exposure_path),
        bundle,
        bundle_content: content,
        node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache as repo_cache;
    use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint};
    use std::fs;
    use tempfile::TempDir;

    const CAMERA_CONTRACT: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "rgb_camera", tag: "v1" },
        interfaces: {
            services: [
                {
                    name: "video_stream_info",
                    response_message_format: {
                        width: "u32",
                        height: "u32",
                        encoding: "string",
                    },
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
                    goal_service: {
                        request_message_format: { task_description: "string" },
                    },
                    result_service: {
                        response_message_format: { episode_index: "u32" },
                    },
                },
            ],
        },
    }"#;

    fn sha_of(contract: &str) -> String {
        ManifestFingerprint::of_bytes(contract.as_bytes()).to_string()
    }

    fn camera_sha() -> String {
        sha_of(CAMERA_CONTRACT)
    }

    fn exposure_document(member: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "camera_surface", tag: "v1" }},
            server: {{ title: "Camera surface" }},
            targets: {{
                front_camera: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                    services: [
                        {{
                            member: "{member}",
                            tool: "front_camera.info",
                            description: "Report stream parameters.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                    ],
                }},
            }},
        }}"#,
            camera_sha()
        )
    }

    fn action_exposure_document() -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "recording_surface", tag: "v1" }},
            server: {{ title: "Recording surface" }},
            targets: {{
                recorder: {{
                    contract: {{ name: "episode_recording", tag: "v1", sha256: "{}" }},
                    actions: [
                        {{
                            member: "record_episode",
                            tool: "recorder.record_episode",
                            description: "Record one teleoperation episode.",
                            operation: "long_running",
                            safety_sensitive: true,
                            confirmation_required: true,
                            deadline_ms: 900000,
                        }},
                    ],
                }},
            }},
        }}"#,
            sha_of(RECORDING_CONTRACT)
        )
    }

    /// A temp `PeppyDirs` whose contract cache serves the fixture contracts
    /// from on-disk files, plus an exposure document written next to its
    /// would-be artifacts. Returns `(dirs_guard, docs_guard, dirs,
    /// exposure_path)`.
    fn seeded_setup(exposure: &str) -> (TempDir, TempDir, PeppyDirs, PathBuf) {
        let peppy_tmp = TempDir::new().expect("temp peppy home");
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let docs_tmp = TempDir::new().expect("temp docs dir");
        let mut entries = Vec::new();
        for (name, contract) in [
            ("rgb_camera", CAMERA_CONTRACT),
            ("episode_recording", RECORDING_CONTRACT),
        ] {
            let contract_path = docs_tmp.path().join(format!("{name}_v1.json5"));
            fs::write(&contract_path, contract).expect("write contract");
            entries.push(repo_cache::ContractCacheEntry {
                contract_name: ItemName::parse(name).expect("valid name"),
                tag: ItemTag::parse("v1").expect("valid tag"),
                sha256: ManifestFingerprint::of_bytes(contract.as_bytes()),
                origin: repo_cache::EntryOrigin::Fs {
                    path: contract_path,
                },
                repo_id: 0,
            });
        }
        let cache_json = serde_json5::to_string(&entries).expect("serialize cache");
        fs::write(repo_cache::contracts_repo_cache_path(&dirs), cache_json)
            .expect("write cache file");

        let exposure_path = docs_tmp.path().join("camera_surface.json5");
        fs::write(&exposure_path, exposure).expect("write exposure");
        (peppy_tmp, docs_tmp, dirs, exposure_path)
    }

    #[test]
    fn publishes_the_bundle_and_the_node_next_to_the_document() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let published =
            publish_exposure(&exposure_path, &dirs, &|_| {}).expect("the exposure publishes");
        assert_eq!(published.bundle_path, exposure_bundle_path(&exposure_path));
        assert_eq!(published.bundle.node.name, "camera_surface_mcp");
        assert_eq!(published.bundle.tools.len(), 1);

        let committed = fs::read_to_string(&published.bundle_path).expect("bundle file exists");
        assert_eq!(committed, published.bundle.to_json_string());
        assert!(
            committed.ends_with('\n'),
            "committed bundles end with a newline"
        );

        let node_dir = published.node_dir;
        assert_eq!(node_dir, exposure_path.with_file_name("camera_surface_mcp"));
        assert_eq!(published.node_file_count, 6);
        for path in [
            "peppy.json5",
            "Cargo.toml",
            ".gitignore",
            "src/main.rs",
            "src/bridges.rs",
        ] {
            assert!(node_dir.join(path).is_file(), "{path} should be published");
        }
        let embedded_bundle =
            fs::read_to_string(node_dir.join("src/bundle.json")).expect("embedded bundle exists");
        assert_eq!(
            embedded_bundle, committed,
            "the node embeds the exact published bundle bytes"
        );
    }

    #[test]
    fn check_passes_right_after_publication() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        publish_exposure(&exposure_path, &dirs, &|_| {}).expect("publishes");
        let drifts = check_exposure(&exposure_path, &dirs, &|_| {}).expect("check runs");
        assert_eq!(drifts, Vec::new());
    }

    #[test]
    fn check_reports_missing_artifacts() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let drifts = check_exposure(&exposure_path, &dirs, &|_| {}).expect("check runs");
        assert!(
            drifts
                .iter()
                .any(|d| matches!(d, ExposureDrift::BundleMissing { .. })),
            "{drifts:?}"
        );
        let node_missing = drifts
            .iter()
            .filter(|d| matches!(d, ExposureDrift::NodeFileMissing { .. }))
            .count();
        assert_eq!(node_missing, 6, "every node file is reported: {drifts:?}");
    }

    #[test]
    fn check_reports_a_tampered_bundle_and_node_file() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let published = publish_exposure(&exposure_path, &dirs, &|_| {}).expect("publishes");
        let mut committed = fs::read_to_string(&published.bundle_path).expect("bundle exists");
        committed.push_str("{}\n");
        fs::write(&published.bundle_path, committed).expect("tamper bundle");
        let main_rs = published.node_dir.join("src/main.rs");
        let mut main_content = fs::read_to_string(&main_rs).expect("main.rs exists");
        main_content.push_str("// edited by hand\n");
        fs::write(&main_rs, main_content).expect("tamper node");

        let drifts = check_exposure(&exposure_path, &dirs, &|_| {}).expect("check runs");
        assert!(
            drifts
                .iter()
                .any(|d| matches!(d, ExposureDrift::BundleOutdated { .. })),
            "{drifts:?}"
        );
        assert!(
            drifts.iter().any(|d| matches!(
                d,
                ExposureDrift::NodeFileOutdated { path } if path.ends_with("src/main.rs")
            )),
            "{drifts:?}"
        );
        assert_eq!(drifts.len(), 2, "untouched files do not drift: {drifts:?}");
    }

    #[test]
    fn an_action_exposure_publishes_the_node_too() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&action_exposure_document());
        let published = publish_exposure(&exposure_path, &dirs, &|_| {}).expect("publishes");
        assert_eq!(published.bundle.tasks.len(), 1);
        assert_eq!(
            published.node_dir,
            exposure_path.with_file_name("recording_surface_mcp")
        );
        assert_eq!(published.node_file_count, 6);
        let bridges = fs::read_to_string(published.node_dir.join("src/bridges.rs"))
            .expect("bridges are published");
        assert!(
            bridges.contains("fire_goal"),
            "the action bridge is published: {bridges}"
        );

        let drifts = check_exposure(&exposure_path, &dirs, &|_| {}).expect("check runs");
        assert_eq!(drifts, Vec::new());
    }

    #[test]
    fn an_invalid_exposure_is_refused_with_its_violations() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("set_exposure"));
        let error = publish_exposure(&exposure_path, &dirs, &|_| {})
            .expect_err("an absent member is refused");
        assert!(error.contains("set_exposure"), "{error}");
        assert!(error.contains("declares no such service"), "{error}");
        assert!(
            !exposure_bundle_path(&exposure_path).exists(),
            "a refused exposure publishes nothing"
        );
    }

    #[test]
    fn an_unresolvable_pin_is_refused() {
        let exposure = exposure_document("video_stream_info").replace(
            &camera_sha(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let (_dirs_guard, _docs_guard, dirs, exposure_path) = seeded_setup(&exposure);
        let error = publish_exposure(&exposure_path, &dirs, &|_| {})
            .expect_err("a pin the caches cannot serve is refused");
        assert!(error.contains("rgb_camera"), "{error}");
    }
}
