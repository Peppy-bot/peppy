use super::NodeStack;
use config::peppy_config::Deployment;

use super::types::{DeploymentMap, ResolvedNodeSource};
use crate::error::{Error, Result};

pub fn resolve_local_deployment(
    deployment: &Deployment,
    node_stack: &NodeStack,
) -> Result<DeploymentMap> {
    let entity = node_stack
        .find(deployment.name.as_str(), &deployment.tag)
        .ok_or_else(|| Error::NodeNotFound(deployment.name.to_string()))?;

    let node = entity.into_config();
    let node_source = ResolvedNodeSource::new(deployment.source.clone(), node);
    Ok(DeploymentMap::new(deployment.clone(), node_source))
}

#[cfg(test)]
mod tests {
    use config::node::Name;
    use config::peppy_config::DeploymentNodeSource;

    use super::*;
    use crate::deployment::NodeStack;
    use crate::error::Error;

    #[test]
    fn resolve_local_deployment_success() {
        let config = sample_config_camera();
        let deployment = sample_deployment();
        let stack = NodeStack::new();
        stack.push_config_with_instance_id(config.clone(), Name::new("test-instance").unwrap());

        let map = resolve_local_deployment(&deployment, &stack).expect("local deployment resolves");

        assert_eq!(map.deployment().name, deployment.name);
        assert_eq!(map.deployment().tag, deployment.tag);

        let node_source = map.node_source();
        assert!(matches!(
            node_source.source(),
            Some(DeploymentNodeSource::Local(_))
        ));
        assert_eq!(
            node_source.node().manifest.name.as_str(),
            config.manifest.name.as_str()
        );
    }

    #[test]
    fn resolve_local_deployment_missing_node() {
        let empty_stack = NodeStack::new();
        let err = resolve_local_deployment(&sample_deployment(), &empty_stack)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");

        let config = sample_config_lidar();
        let stack = NodeStack::new();
        stack.push_config_with_instance_id(config, Name::new("test-instance").unwrap());

        let err = resolve_local_deployment(&sample_deployment(), &stack)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");
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
