use super::ResolvedNode;
use crate::error::{Error, Result};
use config::consts::NODE_CONFIG_FILE;
use config::node::{NodeConfig, NodeConfigParser, VariantConfigParser};
use config::peppy_config::DeploymentLocalSource;
use config::source::DeploymentSource;
use std::path::{Path, PathBuf};

/// Resolves the default variant for a node config on the local filesystem.
///
/// Reads the variant's config file from disk and merges the root's
/// manifest/interfaces with the variant's execution.
fn resolve_default_variant_local(
    raw_config: &config::node::RawNodeConfig,
    variant_source: &DeploymentSource,
    root_path: &Path,
) -> Result<ResolvedNode> {
    let local_source = match variant_source {
        DeploymentSource::Local(local) => local,
        DeploymentSource::Git(_) | DeploymentSource::Url(_) => {
            return Err(Error::NotImplemented(
                "non-local default variant sources in local deployments",
            ));
        }
    };

    let target = if local_source.local.is_relative() {
        root_path.join(&local_source.local)
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

    Ok(ResolvedNode {
        config: NodeConfig {
            schema_version: raw_config.schema_version,
            manifest: raw_config.manifest.clone(),
            interfaces: raw_config.interfaces.clone(),
            execution: variant_config.execution,
        },
        root_path: variant_dir,
    })
}

pub fn resolve_local_deployment(
    base_dir: &Path,
    spec: &DeploymentLocalSource,
) -> Result<ResolvedNode> {
    let target = if spec.local.is_absolute() {
        spec.local.clone()
    } else {
        base_dir.join(&spec.local)
    };

    let (root_path, config_path) = if target.is_dir() {
        (target.clone(), target.join(NODE_CONFIG_FILE))
    } else {
        let parent = target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        (parent, target.clone())
    };

    if !config_path.is_file() {
        return Err(Error::FileNotFound(config_path));
    }

    let raw_config = NodeConfigParser::from_path(&config_path)?;

    if let Some(source) = raw_config.manifest.default_variant_source() {
        resolve_default_variant_local(&raw_config, source, &root_path)
    } else {
        Ok(ResolvedNode {
            config: raw_config.into_resolved()?,
            root_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_local_deployment_default_variant_merges_execution() {
        let dir = tempfile::tempdir().expect("temp dir");
        let node_dir = dir.path().join("variant_node");
        std::fs::create_dir_all(&node_dir).expect("create node dir");
        std::fs::write(
            node_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "variant_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/default" } },
                    ],
                },
            }"#,
        )
        .expect("write node config");

        let variant_dir = node_dir.join("variants").join("default");
        std::fs::create_dir_all(&variant_dir).expect("create variant dir");
        std::fs::write(
            variant_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                execution: {
                    language: "rust",
                    start_cmd: ["./target/release/variant_node"]
                }
            }"#,
        )
        .expect("write variant config");

        let spec = DeploymentLocalSource {
            local: PathBuf::from("./variant_node"),
        };
        let resolved = resolve_local_deployment(dir.path(), &spec)
            .expect("default variant should resolve successfully");

        assert_eq!(resolved.config.manifest.name.as_str(), "variant_node");
        assert_eq!(resolved.config.manifest.tag, "0.1.0");
        assert_eq!(
            resolved.config.execution.start_cmd,
            Some(vec!["./target/release/variant_node".to_string()])
        );
        assert_eq!(resolved.root_path, variant_dir);
    }

    #[test]
    fn resolve_local_deployment_default_variant_missing_file_returns_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let node_dir = dir.path().join("variant_node");
        std::fs::create_dir_all(&node_dir).expect("create node dir");
        std::fs::write(
            node_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "variant_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "default", source: { local: "./variants/default" } },
                    ],
                },
            }"#,
        )
        .expect("write node config");

        let spec = DeploymentLocalSource {
            local: PathBuf::from("./variant_node"),
        };
        let err = resolve_local_deployment(dir.path(), &spec)
            .expect_err("should fail when variant file is missing");

        let msg = err.to_string();
        assert!(
            msg.contains("peppy.json5") || msg.contains("No such file"),
            "expected file-not-found error, got: {msg}"
        );
    }

    #[test]
    fn resolve_local_deployment_non_default_variant_succeeds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let node_dir = dir.path().join("gpu_node");
        std::fs::create_dir_all(&node_dir).expect("create node dir");
        std::fs::write(
            node_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: {
                    name: "gpu_node",
                    tag: "0.1.0",
                    variants: [
                        { name: "gpu", source: { local: "./variants/gpu" } },
                    ],
                },
                execution: {
                    language: "rust",
                    start_cmd: ["./target/release/gpu_node"]
                }
            }"#,
        )
        .expect("write node config");

        let spec = DeploymentLocalSource {
            local: PathBuf::from("./gpu_node"),
        };
        let resolved =
            resolve_local_deployment(dir.path(), &spec).expect("non-default variant resolves");

        assert_eq!(resolved.config.manifest.name.as_str(), "gpu_node");
    }

    #[test]
    fn resolve_local_deployment_dir_success() {
        let dir = tempfile::tempdir().expect("temp dir");
        let node_dir = dir.path().join("uvc_camera");
        std::fs::create_dir_all(&node_dir).expect("create node dir");
        std::fs::write(
            node_dir.join(NODE_CONFIG_FILE),
            r#"{
                schema_version: 1,
                manifest: { name: "uvc_camera",
                    tag: "0.1.0" },

                execution: {
                    language: "rust",
                    start_cmd: ["./target/release/uvc_camera"]
                }
            }"#,
        )
        .expect("write node config");

        let spec = DeploymentLocalSource {
            local: PathBuf::from("./uvc_camera"),
        };
        let resolved =
            resolve_local_deployment(dir.path(), &spec).expect("local deployment resolves");

        assert_eq!(resolved.config.manifest.name.as_str(), "uvc_camera");
        assert_eq!(resolved.config.manifest.tag, "0.1.0");
        assert_eq!(resolved.root_path, node_dir);
    }
}
