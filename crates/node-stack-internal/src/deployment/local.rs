use super::types::{DeploymentMap, NodeSource};

use config::{Deployment, NodeConfig};

use crate::error::{Error, Result};

pub fn resolve_local_deployment(
    deployment: &Deployment,
    nodes: &[NodeConfig],
) -> Result<DeploymentMap> {
    let node = nodes
        .iter()
        .find(|node| {
            node.manifest.name.as_str() == deployment.name && node.manifest.tag == deployment.tag
        })
        .cloned()
        .ok_or_else(|| Error::NodeNotFound(deployment.name.clone()))?;

    let node_source = NodeSource::new(deployment.source.clone(), node);
    Ok(DeploymentMap::new(deployment.clone(), node_source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use config::{Deployment, NodeConfig};

    #[test]
    fn resolve_local_deployment_success() {
        let node = sample_node_camera();
        let deployment = sample_deployment();
        let nodes = vec![node.clone()];

        let map = resolve_local_deployment(&deployment, &nodes).expect("local deployment resolves");

        assert_eq!(map.deployment().name, deployment.name);
        assert_eq!(map.deployment().tag, deployment.tag);

        let node_source = map.node_source();
        assert!(node_source.source().is_local());
        assert_eq!(
            node_source.node().manifest.name.as_str(),
            node.manifest.name.as_str()
        );
    }

    #[test]
    fn resolve_local_deployment_missing_node() {
        let err = resolve_local_deployment(&sample_deployment(), &[])
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");

        let node = sample_node_lidar();
        let nodes = vec![node];

        let err = resolve_local_deployment(&sample_deployment(), &nodes)
            .expect_err("should report missing local node");

        let Error::NodeNotFound(name) = err else {
            panic!("unexpected error");
        };
        assert_eq!(name, "uvc_camera");
    }

    fn sample_node_camera() -> NodeConfig {
        serde_json5::from_str(
            r#"{
                manifest: {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    launch_cmd: ["cargo", "run", "--release"]
                }
            }"#,
        )
        .expect("valid node json5")
    }

    fn sample_node_lidar() -> NodeConfig {
        serde_json5::from_str(
            r#"{
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
                instances: [
                    {
                        namespace: "/"
                    }
                ]
            }"#,
        )
        .expect("valid deployment json5")
    }
}
