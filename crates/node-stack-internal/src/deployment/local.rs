use super::{NodeStack, ResolvedNode};
use config::peppy_config::Deployment;

use crate::error::{Error, Result};

pub fn resolve_local_deployment(
    deployment: &Deployment,
    node_stack: &NodeStack,
) -> Result<ResolvedNode> {
    let entity = node_stack
        .find(deployment.name.as_str(), &deployment.tag)
        .ok_or_else(|| Error::NodeNotFound(deployment.name.to_string()))?;

    Ok(ResolvedNode {
        config: entity.config().clone(),
        root_path: entity.root_path().to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use config::node::Name;

    use super::*;
    use crate::deployment::NodeStack;
    use crate::error::Error;

    #[test]
    fn resolve_local_deployment_success() {
        let config = sample_config_camera();
        let config_manifest = config.manifest.clone();
        let deployment = sample_deployment();
        let root_path = PathBuf::from("/tmp");
        let stack = NodeStack::new(master_node_config(), None, root_path.clone());
        stack
            .push_config(config, false, root_path.clone())
            .expect("config has no dependencies");
        stack
            .add_instance(
                config_manifest.name.as_str(),
                &config_manifest.tag,
                Some(&Name::new("test-instance").unwrap()),
            )
            .expect("should spawn instance");

        let resolved =
            resolve_local_deployment(&deployment, &stack).expect("local deployment resolves");

        assert_eq!(
            resolved.config.manifest.name.as_str(),
            deployment.name.as_str()
        );
        assert_eq!(resolved.config.manifest.tag, deployment.tag);
        assert_eq!(
            resolved.config.manifest.name.as_str(),
            config_manifest.name.as_str()
        );
        assert_eq!(resolved.root_path, root_path);
    }

    #[test]
    fn resolve_local_deployment_missing_node() {
        let root_path = PathBuf::from("/tmp");
        let stack = NodeStack::new(master_node_config(), None, root_path.clone());
        let err = resolve_local_deployment(&sample_deployment(), &stack)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");

        let config = sample_config_lidar();
        let stack = NodeStack::new(master_node_config(), None, root_path.clone());
        stack
            .push_config(config, false, root_path)
            .expect("config has no dependencies");

        let err = resolve_local_deployment(&sample_deployment(), &stack)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");
    }

    fn master_node_config() -> config::node::NodeConfig {
        serde_json5::from_str(
            r#"{
                schema_version: 1,
                manifest: {
                    name: "master",
                    tag: "1.0.0",
                    start_cmd: ["cargo", "run"]
                }
            }"#,
        )
        .expect("valid master node json5")
    }

    fn sample_config_camera() -> config::node::NodeConfig {
        serde_json5::from_str(
            r#"{
                schema_version: 1,
                manifest: {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    start_cmd: ["cargo", "run", "--release"]
                }
            }"#,
        )
        .expect("valid node json5")
    }

    fn sample_config_lidar() -> config::node::NodeConfig {
        serde_json5::from_str(
            r#"{
                schema_version: 1,
                manifest: {
                    name: "lidar",
                    tag: "0.1.0",
                    start_cmd: ["cargo", "run", "--release"]
                }
            }"#,
        )
        .expect("valid node json5")
    }

    fn sample_deployment() -> Deployment {
        serde_json5::from_str(
            r#"{
                name: "uvc_camera",
                source: "file:///tmp/peppy_node",
                tag: "0.1.0",
                instances: []
            }"#,
        )
        .expect("valid deployment json5")
    }
}
