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

    let node = NodeConfigParser::from_path(&config_path)?;

    Ok(ResolvedNode {
        config: node,
        root_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
