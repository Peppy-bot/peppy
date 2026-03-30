use std::path::{Path, PathBuf};

use crate::error::Result;
use config::consts::NODE_CONFIG_FILE;
use config::node::{NodeConfig, RawNodeConfig, VariantConfig, VariantConfigParser};
use config::peppy_config::DeploymentLocalSource;
use config::source::DeploymentSource;

/// Resolves the default variant for a node config, merging the root's
/// manifest/interfaces with the variant's execution.
///
/// The `resolve_local` closure handles `DeploymentSource::Local` variants,
/// which differ between git-tree-based and filesystem-based resolution.
/// `DeploymentSource::Git` and `DeploymentSource::Url` variants are handled
/// uniformly by this function.
pub(super) fn resolve_default_variant(
    raw_config: RawNodeConfig,
    variant_source: &DeploymentSource,
    resolve_local: impl FnOnce(&DeploymentLocalSource) -> Result<(VariantConfig, PathBuf)>,
    added_nodes_dir: &Path,
) -> Result<(NodeConfig, PathBuf)> {
    let (variant_config, variant_root_path) = match variant_source {
        DeploymentSource::Local(local_source) => resolve_local(local_source)?,
        DeploymentSource::Git(git_spec) => {
            let repo_dir = super::git::build_repo_cache_path(added_nodes_dir, &git_spec.repo);
            let repo = super::git::ensure_repository(&repo_dir, &git_spec.repo)?;
            super::git::fetch_repository(&repo)?;
            let commit = super::git::find_commit_for_tag(&repo, &git_spec.ref_)?;
            let tree = commit.tree()?;
            let config_path = super::git::node_config_path(&git_spec.path);
            let content = super::git::read_blob_from_tree(&repo, &tree, &config_path)?;
            let root = repo_dir.join(config_path.parent().unwrap_or_else(|| Path::new("")));
            (VariantConfigParser::from_content(&content)?, root)
        }
        DeploymentSource::Url(url_spec) => {
            let cache_dir = super::url::ensure_url_source(added_nodes_dir, url_spec)?;
            let config_path = cache_dir.join(NODE_CONFIG_FILE);
            (VariantConfigParser::from_path(&config_path)?, cache_dir)
        }
    };

    Ok((
        NodeConfig {
            schema_version: raw_config.schema_version,
            manifest: raw_config.manifest,
            interfaces: raw_config.interfaces,
            execution: variant_config.execution,
        },
        variant_root_path,
    ))
}

/// Resolves a `DeploymentSource::Local` variant from the filesystem.
///
/// Used by both `local.rs` and `url.rs` resolvers where the variant config
/// file is on disk (as opposed to the git resolver which reads from a git tree).
pub(super) fn resolve_local_variant_from_path(
    local_source: &DeploymentLocalSource,
    root_dir: &Path,
) -> Result<(VariantConfig, PathBuf)> {
    let target = if local_source.local.is_relative() {
        root_dir.join(&local_source.local)
    } else {
        local_source.local.clone()
    };

    let (variant_dir, variant_config_path) = if target.is_file() {
        let parent = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        (parent, target)
    } else {
        (target.clone(), target.join(NODE_CONFIG_FILE))
    };

    let variant_config = VariantConfigParser::from_path(&variant_config_path)?;
    Ok((variant_config, variant_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;

    #[test]
    fn resolve_default_variant_delegates_to_local_closure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_dir = dir.path().join("node");
        std::fs::create_dir_all(&root_dir).expect("create root dir");
        std::fs::write(
            root_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "test_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/default" } },
                    ],
                },
            }"#,
        )
        .expect("write root config");

        let variant_dir = root_dir.join("variants").join("default");
        std::fs::create_dir_all(&variant_dir).expect("create variant dir");
        std::fs::write(
            variant_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: {
                    language: "rust",
                    start_cmd: ["./target/release/test_node"]
                }
            }"#,
        )
        .expect("write variant config");

        let raw_config =
            NodeConfigParser::from_path(root_dir.join(NODE_CONFIG_FILE)).expect("parse root");
        let source = raw_config
            .manifest
            .default_variant_source()
            .cloned()
            .expect("has default variant");

        let added_nodes_dir = dir.path().join("added_nodes");
        std::fs::create_dir_all(&added_nodes_dir).expect("create added_nodes dir");

        let (node, root_path) = resolve_default_variant(
            raw_config,
            &source,
            |local_source| resolve_local_variant_from_path(local_source, &root_dir),
            &added_nodes_dir,
        )
        .expect("resolve variant");

        assert_eq!(node.manifest.name.as_str(), "test_node");
        assert_eq!(node.manifest.tag, "0.1.0");
        assert_eq!(
            node.execution.start_cmd,
            Some(vec!["./target/release/test_node".to_string()])
        );
        assert_eq!(root_path, variant_dir);
    }
}
