use super::ResolvedNode;
use crate::error::{Error, Result};
use config::consts::NODE_CONFIG_FILE;
use config::node::NodeConfigParser;
use config::peppy_config::DeploymentLocalSource;
use std::path::{Path, PathBuf};

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

    let node = NodeConfigParser::from_path(&config_path)?.into_resolved()?;

    Ok(ResolvedNode {
        config: node,
        root_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_local_deployment_default_variant_returns_error() {
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
            .expect_err("should fail for default variant config");

        let msg = err.to_string();
        assert!(
            msg.contains("execution"),
            "expected missing-execution error, got: {msg}"
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
