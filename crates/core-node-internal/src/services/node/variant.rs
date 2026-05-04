use super::add::{CleanupDir, download_and_extract_http_source};
use super::{checkout_repo_ref, sanitize_repo_path};
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::{NodeConfig, ParsedNodeConfig, VariantConfigParser};
use config::source::DeploymentSource;
use core_node_api::encoding::NodeSource;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) struct ResolvedVariant {
    /// The merged NodeConfig: root manifest + root interfaces + variant execution.
    pub(crate) merged_config: NodeConfig,
    /// Path to the variant's source directory.
    pub(crate) variant_source_path: PathBuf,
    /// Whether the variant's source directory is local (not cloned/downloaded).
    pub(crate) source_is_local: bool,
    /// Directory to clean up after variant resolution (git clone / http download).
    pub(crate) cleanup_dir: Option<PathBuf>,
    /// True when the variant's config defined a `manifest` section that was ignored.
    pub(crate) manifest_ignored: bool,
}

/// Returns a short display label for a variant source, used in log/feedback messages.
pub(crate) fn variant_label(variant: &NodeSource) -> String {
    match variant {
        NodeSource::Fs(name) => name.to_string_lossy().to_string(),
        NodeSource::Git {
            repo_url,
            repo_path,
            ..
        } => format!("git:{}::{}", repo_url, repo_path),
        NodeSource::Http { url, .. } => url.to_string(),
        NodeSource::RepoNode { name, tag, .. } => format!("{name}:{tag}"),
    }
}

/// Resolves a variant from a [`NodeSource`], parsing its config, validating
/// interfaces against the root, and merging the root's manifest/interfaces
/// with the variant's execution.
///
/// - `NodeSource::Fs` is treated as a variant **name** (looked up in the root
///   manifest's `variants` array, then its `DeploymentSource` is resolved).
/// - `NodeSource::Git` and `NodeSource::Http` are resolved directly — the
///   manifest lookup is skipped.
pub(crate) async fn resolve_variant(
    variant: &NodeSource,
    root_config: &ParsedNodeConfig,
    root_source_path: &Path,
    peppy_dirs: &PeppyDirs,
    deadline: Option<Instant>,
) -> std::result::Result<ResolvedVariant, String> {
    let label = variant_label(variant);

    // Resolve the variant's source to a directory path and parse its config.
    let (variant_source_path, variant_config, source_is_local, cleanup_dir) = match variant {
        // Name-based lookup: find in root manifest, then resolve its DeploymentSource.
        NodeSource::Fs(name) => {
            let variant_name = name.to_string_lossy();
            let matched = root_config.find_variant(&variant_name).ok_or_else(|| {
                format!(
                    "variant '{}' not found in manifest of node '{}:{}'",
                    variant_name,
                    root_config.manifest_name(),
                    root_config.manifest_tag(),
                )
            })?;

            resolve_variant_deployment_source(
                &matched.source,
                &label,
                root_source_path,
                peppy_dirs,
                deadline,
            )
            .await?
        }
        // Direct git source — skip manifest lookup.
        NodeSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => {
            let repo_relative_path = sanitize_repo_path(repo_path)?;

            let temp_dir = tempfile::tempdir()
                .map_err(|e| format!("Failed to create temporary directory: {}", e))?;
            let dest = temp_dir.path().to_path_buf();

            let clone_dest = dest.clone();
            let clone_repo_url = repo_url.to_bstring().to_string();
            let clone_repo_ref = repo_ref.clone();
            let clone_deadline = deadline;
            tokio::task::spawn_blocking(move || {
                let repo =
                    super::clone_repo_with_deadline(&clone_repo_url, &clone_dest, clone_deadline)?;
                if let Some(repo_ref) = clone_repo_ref.as_deref() {
                    checkout_repo_ref(&repo, repo_ref)
                        .map_err(|e| format!("Failed to checkout git ref '{}': {}", repo_ref, e))?;
                }
                Ok::<_, String>(())
            })
            .await
            .map_err(|e| format!("Failed to join git clone task: {}", e))??;

            let checkout_dir = temp_dir.keep();
            let node_root_dir = checkout_dir.join(&repo_relative_path);
            let config_path = node_root_dir.join(NODE_CONFIG_FILE);
            let variant_config = match VariantConfigParser::from_path(&config_path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    std::fs::remove_dir_all(&checkout_dir).ok();
                    return Err(format!(
                        "Failed to parse variant config at {}: {}",
                        config_path.display(),
                        e
                    ));
                }
            };
            (node_root_dir, variant_config, false, Some(checkout_dir))
        }
        NodeSource::RepoNode { .. } => {
            return Err("repo-node sources are not valid variant selectors".to_owned());
        }
        // Direct HTTP source — skip manifest lookup.
        NodeSource::Http { url, sha256 } => {
            let extracted =
                download_and_extract_http_source(url, peppy_dirs.clone(), sha256.clone()).await?;
            let mut http_guard = CleanupDir::new(extracted.cleanup_dir);
            let config_path = extracted.source_path.join(NODE_CONFIG_FILE);
            let variant_config = VariantConfigParser::from_path(&config_path).map_err(|e| {
                format!(
                    "Failed to parse variant config at {}: {}",
                    config_path.display(),
                    e
                )
            })?;
            (
                extracted.source_path,
                variant_config,
                false,
                http_guard.take(),
            )
        }
    };

    // Guard ensures cleanup_dir is removed if we exit early (e.g. validation error).
    let mut cleanup_guard = CleanupDir::new(cleanup_dir);

    // Validate interfaces and merge root config with variant execution.
    let merged = root_config.merge_variant(variant_config, &label)?;

    // Defuse the guard — caller takes ownership of cleanup responsibility.
    let cleanup_dir = cleanup_guard.take();

    Ok(ResolvedVariant {
        merged_config: merged.config,
        variant_source_path,
        source_is_local,
        cleanup_dir,
        manifest_ignored: merged.manifest_ignored,
    })
}

/// Validates that a local variant source path is relative and contained within
/// the root source directory. Returns the canonicalized path on success.
fn validate_local_source_path(
    root_source_path: &Path,
    local_path: &Path,
    label: &str,
) -> std::result::Result<PathBuf, String> {
    if local_path.is_absolute() {
        return Err(format!(
            "Variant '{}' local source path must be relative, got: {}",
            label,
            local_path.display()
        ));
    }
    let candidate = root_source_path.join(local_path);
    if !candidate.exists() {
        return Err(format!(
            "Variant '{}' source directory does not exist: {}",
            label,
            candidate.display()
        ));
    }
    let root_canon = fs::canonicalize(root_source_path).map_err(|e| {
        format!(
            "Failed to resolve root source path {}: {}",
            root_source_path.display(),
            e
        )
    })?;
    let candidate_canon = fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "Failed to resolve variant '{}' source path {}: {}",
            label,
            candidate.display(),
            e
        )
    })?;
    if !candidate_canon.starts_with(&root_canon) {
        return Err(format!(
            "Variant '{}' source path escapes root directory: {}",
            label,
            candidate.display()
        ));
    }
    Ok(candidate_canon)
}

/// Resolves a variant from a [`DeploymentSource`] found in the root manifest.
/// This is used by the name-based lookup path.
async fn resolve_variant_deployment_source(
    deployment: &DeploymentSource,
    label: &str,
    root_source_path: &Path,
    peppy_dirs: &PeppyDirs,
    deadline: Option<Instant>,
) -> std::result::Result<(PathBuf, config::node::VariantConfig, bool, Option<PathBuf>), String> {
    match deployment {
        DeploymentSource::Local(local) => {
            let path = validate_local_source_path(root_source_path, &local.local, label)?;
            let config_path = path.join(NODE_CONFIG_FILE);
            let variant_config = VariantConfigParser::from_path(&config_path).map_err(|e| {
                format!(
                    "Failed to parse variant '{}' config at {}: {}",
                    label,
                    config_path.display(),
                    e
                )
            })?;
            Ok((path, variant_config, true, None))
        }
        DeploymentSource::Git(git) => {
            gix_url::Url::try_from(git.repo.as_str())
                .map_err(|e| format!("invalid variant git URL: {}", e))?;
            let repo_relative_path = sanitize_repo_path(&git.path)?;

            let temp_dir = tempfile::tempdir()
                .map_err(|e| format!("Failed to create temporary directory: {}", e))?;
            let dest = temp_dir.path().to_path_buf();

            let clone_dest = dest.clone();
            let clone_repo_url = git.repo.clone();
            let clone_repo_ref = Some(git.ref_.clone());
            let clone_deadline = deadline;
            tokio::task::spawn_blocking(move || {
                let repo =
                    super::clone_repo_with_deadline(&clone_repo_url, &clone_dest, clone_deadline)?;
                if let Some(repo_ref) = clone_repo_ref.as_deref() {
                    checkout_repo_ref(&repo, repo_ref)
                        .map_err(|e| format!("Failed to checkout git ref '{}': {}", repo_ref, e))?;
                }
                Ok::<_, String>(())
            })
            .await
            .map_err(|e| format!("Failed to join git clone task: {}", e))??;

            let checkout_dir = temp_dir.keep();
            let node_root_dir = checkout_dir.join(&repo_relative_path);
            let config_path = node_root_dir.join(NODE_CONFIG_FILE);
            let variant_config = match VariantConfigParser::from_path(&config_path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    std::fs::remove_dir_all(&checkout_dir).ok();
                    return Err(format!(
                        "Failed to parse variant '{}' config at {}: {}",
                        label,
                        config_path.display(),
                        e
                    ));
                }
            };
            Ok((node_root_dir, variant_config, false, Some(checkout_dir)))
        }
        DeploymentSource::Url(url_source) => {
            let url = url::Url::parse(&url_source.url)
                .map_err(|e| format!("invalid variant URL: {}", e))?;
            let extracted = download_and_extract_http_source(
                &url,
                peppy_dirs.clone(),
                Some(url_source.sha256.clone()),
            )
            .await?;
            let config_path = extracted.source_path.join(NODE_CONFIG_FILE);
            let variant_config = match VariantConfigParser::from_path(&config_path) {
                Ok(cfg) => cfg,
                Err(e) => {
                    if let Some(ref dir) = extracted.cleanup_dir {
                        std::fs::remove_dir_all(dir).ok();
                    }
                    return Err(format!(
                        "Failed to parse variant '{}' config at {}: {}",
                        label,
                        config_path.display(),
                        e
                    ));
                }
            };
            Ok((
                extracted.source_path,
                variant_config,
                false,
                extracted.cleanup_dir,
            ))
        }
        DeploymentSource::Repo(_) => Err(format!(
            "variant '{label}' uses a repo-backed source, which is not supported inside manifest variant entries; use a git, url, or name variant source instead"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn validate_local_source_path_relative_inside_root_succeeds() {
        let root = tempdir().unwrap();
        let subdir = root.path().join("my_variant");
        fs::create_dir(&subdir).unwrap();

        let result = validate_local_source_path(root.path(), Path::new("my_variant"), "v1");
        let resolved = result.unwrap();
        assert_eq!(resolved, fs::canonicalize(&subdir).unwrap());
    }

    #[test]
    fn validate_local_source_path_rejects_parent_dir_escape() {
        let root = tempdir().unwrap();
        let inner = root.path().join("inner");
        fs::create_dir(&inner).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();

        let result = validate_local_source_path(&inner, Path::new("../outside"), "v1");
        let err = result.unwrap_err();
        assert!(
            err.contains("escapes root directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_local_source_path_rejects_absolute_path() {
        let root = tempdir().unwrap();

        let result = validate_local_source_path(root.path(), Path::new("/etc"), "v1");
        let err = result.unwrap_err();
        assert!(err.contains("must be relative"), "unexpected error: {err}");
    }

    #[test]
    fn validate_local_source_path_nonexistent_path_fails() {
        let root = tempdir().unwrap();

        let result = validate_local_source_path(root.path(), Path::new("nonexistent"), "v1");
        let err = result.unwrap_err();
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn repo_node_rejected_as_variant_selector() {
        // Build a minimal root peppy.json5 so the test can construct a
        // ParsedNodeConfig; its contents don't matter because the
        // RepoNode arm returns `Err` before the root config is touched.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join(NODE_CONFIG_FILE),
            r#"{
                peppy_schema: "nodes_v1",
                manifest: {
                    name: "host",
                    tag: "0.0.0",
                    variants: [
                        { name: "default", source: { local: "./" } }
                    ]
                },
                interfaces: {}
            }"#,
        )
        .unwrap();
        let root_config =
            config::node::NodeConfigParser::from_path(root.join(NODE_CONFIG_FILE)).unwrap();

        let variant = NodeSource::repo_node("some_dep", "1.2.3").unwrap();
        let peppy_dirs = PeppyDirs::default();

        match resolve_variant(&variant, &root_config, root, &peppy_dirs, None).await {
            Err(msg) => assert_eq!(msg, "repo-node sources are not valid variant selectors"),
            Ok(_) => panic!("expected RepoNode variant selector to be rejected"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_local_source_path_rejects_symlink_escape() {
        let root = tempdir().unwrap();
        let inner = root.path().join("inner");
        fs::create_dir(&inner).unwrap();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();

        std::os::unix::fs::symlink(&outside, inner.join("link")).unwrap();

        let result = validate_local_source_path(&inner, Path::new("link"), "v1");
        let err = result.unwrap_err();
        assert!(
            err.contains("escapes root directory"),
            "unexpected error: {err}"
        );
    }
}
