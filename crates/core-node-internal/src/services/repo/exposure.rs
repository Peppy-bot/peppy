//! Exposure-bundle publication: resolve an `mcp_exposure/v1` document's
//! pinned contracts through the local repository caches, validate the
//! exposure against them, and publish (or verify) the committed bundle file
//! next to the document.
//!
//! The validation itself is [`generator::build_exposure_bundle`], a pure
//! function; this module is the repository-facing shell that turns sha256
//! pins into contract bytes and bundle values into committed files.

use crate::services::node::resolve_contract_doc;
use daemon_config::consts::PeppyDirs;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use generator::{ExposureBundle, ResolvedContractDocument, build_exposure_bundle};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A generated bundle and where it was (or would be) published.
#[derive(Debug)]
pub struct GeneratedExposureBundle {
    pub path: PathBuf,
    pub bundle: ExposureBundle,
}

/// How the committed bundle file differs from the one its exposure document
/// produces. At most one drift exists per exposure: the bundle is a single
/// file that either matches, differs, or is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposureBundleDrift {
    Missing { expected_path: String },
    Outdated { path: String },
}

impl std::fmt::Display for ExposureBundleDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { expected_path } => {
                write!(f, "no bundle is committed at {expected_path}")
            }
            Self::Outdated { path } => {
                write!(
                    f,
                    "{path} does not match the bundle its exposure document produces"
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

/// Validates the exposure at `exposure_path` against its pinned contracts
/// (resolved through the local repository caches) and publishes the bundle
/// file next to it.
pub fn generate_exposure_bundle_file(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<GeneratedExposureBundle, String> {
    let (path, bundle, content) = render_bundle(exposure_path, peppy_dirs, on_feedback)?;
    daemon_config::atomic_write::publish_atomic(&path, |tmp| std::fs::write(tmp, &content))
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(GeneratedExposureBundle { path, bundle })
}

/// Verifies the committed bundle file against the one the exposure document
/// produces right now. `Ok(None)` means the committed bundle matches.
pub fn check_exposure_bundle_file(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Option<ExposureBundleDrift>, String> {
    let (path, _, expected) = render_bundle(exposure_path, peppy_dirs, on_feedback)?;
    let committed = match std::fs::read_to_string(&path) {
        Ok(committed) => committed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(ExposureBundleDrift::Missing {
                expected_path: path.display().to_string(),
            }));
        }
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    if committed != expected {
        return Ok(Some(ExposureBundleDrift::Outdated {
            path: path.display().to_string(),
        }));
    }
    Ok(None)
}

/// Parse the exposure, resolve every pinned contract it references, and
/// build the bundle plus its canonical serialized form.
fn render_bundle(
    exposure_path: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<(PathBuf, ExposureBundle, String), String> {
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
    let content = bundle.to_json_string();
    Ok((exposure_bundle_path(exposure_path), bundle, content))
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

    fn camera_sha() -> String {
        ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).to_string()
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

    /// A temp `PeppyDirs` whose contract cache serves the camera contract
    /// from an on-disk file, plus an exposure document written next to a
    /// would-be bundle. Returns `(dirs_guard, docs_guard, dirs,
    /// exposure_path)`.
    fn seeded_setup(exposure: &str) -> (TempDir, TempDir, PeppyDirs, PathBuf) {
        let peppy_tmp = TempDir::new().expect("temp peppy home");
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let docs_tmp = TempDir::new().expect("temp docs dir");
        let contract_path = docs_tmp.path().join("rgb_camera_v1.json5");
        fs::write(&contract_path, CAMERA_CONTRACT).expect("write contract");
        let entry = repo_cache::ContractCacheEntry {
            contract_name: ItemName::parse("rgb_camera").expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()),
            origin: repo_cache::EntryOrigin::Fs {
                path: contract_path,
            },
            repo_id: 0,
        };
        let cache_json = serde_json5::to_string(&vec![entry]).expect("serialize cache");
        fs::write(repo_cache::contracts_repo_cache_path(&dirs), cache_json)
            .expect("write cache file");

        let exposure_path = docs_tmp.path().join("camera_surface.json5");
        fs::write(&exposure_path, exposure).expect("write exposure");
        (peppy_tmp, docs_tmp, dirs, exposure_path)
    }

    #[test]
    fn generates_the_bundle_file_next_to_the_document() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let generated = generate_exposure_bundle_file(&exposure_path, &dirs, &|_| {})
            .expect("the exposure publishes");
        assert_eq!(generated.path, exposure_bundle_path(&exposure_path));
        assert_eq!(generated.bundle.node.name, "camera_surface_mcp");
        assert_eq!(generated.bundle.tools.len(), 1);

        let committed = fs::read_to_string(&generated.path).expect("bundle file exists");
        assert_eq!(committed, generated.bundle.to_json_string());
        assert!(
            committed.ends_with('\n'),
            "committed bundles end with a newline"
        );
    }

    #[test]
    fn check_passes_right_after_generation() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        generate_exposure_bundle_file(&exposure_path, &dirs, &|_| {}).expect("publishes");
        let drift = check_exposure_bundle_file(&exposure_path, &dirs, &|_| {}).expect("check runs");
        assert_eq!(drift, None);
    }

    #[test]
    fn check_reports_a_missing_bundle() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let drift = check_exposure_bundle_file(&exposure_path, &dirs, &|_| {})
            .expect("check runs")
            .expect("the missing bundle is a drift");
        assert!(matches!(drift, ExposureBundleDrift::Missing { .. }));
        assert!(
            drift.to_string().contains("camera_surface.bundle.json"),
            "{drift}"
        );
    }

    #[test]
    fn check_reports_a_tampered_bundle() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("video_stream_info"));
        let generated =
            generate_exposure_bundle_file(&exposure_path, &dirs, &|_| {}).expect("publishes");
        let mut committed = fs::read_to_string(&generated.path).expect("bundle exists");
        committed.push_str("{}\n");
        fs::write(&generated.path, committed).expect("tamper");

        let drift = check_exposure_bundle_file(&exposure_path, &dirs, &|_| {})
            .expect("check runs")
            .expect("the tampered bundle is a drift");
        assert!(matches!(drift, ExposureBundleDrift::Outdated { .. }));
        assert!(
            drift.to_string().contains("does not match the bundle"),
            "{drift}"
        );
    }

    #[test]
    fn an_invalid_exposure_is_refused_with_its_violations() {
        let (_dirs_guard, _docs_guard, dirs, exposure_path) =
            seeded_setup(&exposure_document("set_exposure"));
        let error = generate_exposure_bundle_file(&exposure_path, &dirs, &|_| {})
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
        let error = generate_exposure_bundle_file(&exposure_path, &dirs, &|_| {})
            .expect_err("a pin the caches cannot serve is refused");
        assert!(error.contains("rgb_camera"), "{error}");
    }
}
