use super::add::{CleanupDir, download_and_extract_http_source};
use super::{checkout_repo_ref, sanitize_repo_path};
use crate::encoding::NodeSource;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::{Interfaces, NodeConfig, RawNodeConfig, VariantConfigParser};
use config::source::DeploymentSource;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub(crate) struct ResolvedVariant {
    /// The merged NodeConfig: root manifest + root interfaces + variant execution.
    pub(crate) merged_config: NodeConfig,
    /// Path to the variant's source directory.
    pub(crate) variant_source_path: PathBuf,
    /// Whether to verify codegen fingerprint for the variant.
    pub(crate) verify_codegen_fingerprint: bool,
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
    root_config: &RawNodeConfig,
    root_source_path: &Path,
    peppy_dirs: &PeppyDirs,
    deadline: Option<Instant>,
) -> std::result::Result<ResolvedVariant, String> {
    let label = variant_label(variant);

    // Resolve the variant's source to a directory path and parse its config.
    let (variant_source_path, variant_config, verify_codegen_fingerprint, cleanup_dir) =
        match variant {
            // Name-based lookup: find in root manifest, then resolve its DeploymentSource.
            NodeSource::Fs(name) => {
                let variant_name = name.to_string_lossy();
                let variants = root_config.manifest.variants.as_ref().ok_or_else(|| {
                    format!(
                        "variant '{}' not found: node '{}:{}' does not define any variants",
                        variant_name,
                        root_config.manifest.name.as_str(),
                        root_config.manifest.tag,
                    )
                })?;

                let matched = variants
                    .iter()
                    .find(|v| v.name.as_str() == variant_name.as_ref())
                    .ok_or_else(|| {
                        format!(
                            "variant '{}' not found in manifest of node '{}:{}'",
                            variant_name,
                            root_config.manifest.name.as_str(),
                            root_config.manifest.tag,
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
                    let repo = super::clone_repo_with_deadline(
                        &clone_repo_url,
                        &clone_dest,
                        clone_deadline,
                    )?;
                    if let Some(repo_ref) = clone_repo_ref.as_deref() {
                        checkout_repo_ref(&repo, repo_ref).map_err(|e| {
                            format!("Failed to checkout git ref '{}': {}", repo_ref, e)
                        })?;
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
            // Direct HTTP source — skip manifest lookup.
            NodeSource::Http { url, sha256 } => {
                let extracted =
                    download_and_extract_http_source(url, peppy_dirs.clone(), sha256.clone())
                        .await?;
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

    // If the variant defines interfaces, validate they match the root's interfaces.
    if let Some(ref variant_interfaces) = variant_config.interfaces
        && *variant_interfaces != Interfaces::default()
        && !root_config.interfaces.matches_unordered(variant_interfaces)
    {
        return Err(format!(
            "VariantInterfaceMismatch: variant '{}' defines interfaces that differ from the root node '{}:{}'",
            label,
            root_config.manifest.name.as_str(),
            root_config.manifest.tag,
        ));
    }

    let manifest_ignored = variant_config.manifest.is_some();

    // Build merged config: root's schema_version + manifest + interfaces + variant's execution
    let merged_config = NodeConfig {
        schema_version: root_config.schema_version,
        manifest: root_config.manifest.clone(),
        interfaces: root_config.interfaces.clone(),
        execution: variant_config.execution,
    };

    // Defuse the guard — caller takes ownership of cleanup responsibility.
    let cleanup_dir = cleanup_guard.take();

    Ok(ResolvedVariant {
        merged_config,
        variant_source_path,
        verify_codegen_fingerprint,
        cleanup_dir,
        manifest_ignored,
    })
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
            let path = if local.local.is_relative() {
                root_source_path.join(&local.local)
            } else {
                local.local.clone()
            };
            if !path.exists() {
                return Err(format!(
                    "Variant '{}' source directory does not exist: {}",
                    label,
                    path.display()
                ));
            }
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
    }
}
