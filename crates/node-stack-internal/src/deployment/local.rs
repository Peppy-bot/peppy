use super::NodeStack;
use config::node::NodeConfig;
use config::peppy_config::Deployment;

use crate::error::{Error, Result};

pub fn resolve_local_deployment(
    deployment: &Deployment,
    node_stack: &NodeStack,
) -> Result<NodeConfig> {
    let entity = node_stack
        .find(deployment.name.as_str(), &deployment.tag)
        .ok_or_else(|| Error::NodeNotFound(deployment.name.to_string()))?;

    Ok(entity.into_config())
}

#[cfg(test)]
mod tests {
    use config::node::Name;

    use super::*;
    use crate::deployment::NodeStack;
    use crate::error::Error;

    #[test]
    fn resolve_local_deployment_success() {
        let config = sample_config_camera();
        let deployment = sample_deployment();
        let stack = NodeStack::new(master_node_config(), None);
        stack
            .push_config(&config, Some(&Name::new("test-instance").unwrap()), false)
            .expect("config has no dependencies");

        let node =
            resolve_local_deployment(&deployment, &stack).expect("local deployment resolves");

        assert_eq!(node.manifest.name.as_str(), deployment.name.as_str());
        assert_eq!(node.manifest.tag, deployment.tag);
        assert_eq!(node.manifest.name.as_str(), config.manifest.name.as_str());
    }

    #[test]
    fn resolve_local_deployment_missing_node() {
        let stack = NodeStack::new(master_node_config(), None);
        let err = resolve_local_deployment(&sample_deployment(), &stack)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");

        let stack = NodeStack::new(master_node_config(), None);
        stack
            .push_config(&sample_config_lidar(), None, false)
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
                    tag: "1.0.0"
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
                    launch_cmd: ["cargo", "run", "--release"]
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
                    launch_cmd: ["cargo", "run", "--release"]
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
