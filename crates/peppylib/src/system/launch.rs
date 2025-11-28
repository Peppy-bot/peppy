use crate::error::{Error, Result};
use config::launch_config::LaunchConfig;

const PEPPY_LAUNCH_CONFIG_VAR: &str = "PEPPY_LAUNCH_CONFIG";

pub struct LaunchConfigProcessor {
    launch_config: LaunchConfig,
}

impl LaunchConfigProcessor {
    pub fn new() -> Result<Self> {
        let launch_config_path = std::env::var(PEPPY_LAUNCH_CONFIG_VAR).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: PEPPY_LAUNCH_CONFIG_VAR,
                source,
            }
        })?;
        let launch_config =
            LaunchConfigProcessor::get_peppy_deployment_config(&launch_config_path)?;
        Ok(Self { launch_config })
    }

    fn get_peppy_deployment_config(launch_config_path: &str) -> Result<LaunchConfig> {
        let content = std::fs::read_to_string(launch_config_path).map_err(|source| {
            Error::LaunchConfigRead {
                path: launch_config_path.to_string(),
                source,
            }
        })?;
        serde_json5::from_str(&content).map_err(|source| Error::LaunchConfigParse {
            path: launch_config_path.to_string(),
            source,
        })
    }

    pub fn get_current_instance_id(&self) -> &str {
        self.launch_config.attributes.instance_id.as_str()
    }

    pub fn get_node_name(&self) -> &str {
        self.launch_config.attributes.name.as_str()
    }

    pub fn bound_master_node(&self) -> &str {
        self.launch_config.attributes.bound_master_node.as_str()
    }

    /// Given a `target_instance_tag` in the form of `<master_node>:<node_instance>` (example `self:the_node`),
    /// returns a tuple of `master_node` + `instance_id`.
    ///  - If `self` is given, it's replaced by the current bound master node
    ///  - If nothing is given for the master_node, the current bound master node is used
    ///  - In all other case, split `<master_node>:<node_instance>` into the tuple `<master_node>` and `<node_instance>`
    pub fn get_subscriber_target(&self, target_instance_tag: &str) -> (String, String) {
        let bound_master_node = self.launch_config.attributes.bound_master_node.as_str();
        match target_instance_tag.split_once(':') {
            Some((master_node, node_instance)) => {
                let resolved_master = match master_node {
                    "" | "self" => bound_master_node,
                    other => other,
                };
                (resolved_master.to_string(), node_instance.to_string())
            }
            None => (
                bound_master_node.to_string(),
                target_instance_tag.to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor_with_bound_master(bound_master_node: &str) -> LaunchConfigProcessor {
        let json = r#"{
            deployment: {
                name: "node_name",
                tag: "0.1.0",
                instances: []
            },
            attributes: {
                name: "node_name",
                instance_id: "instance_id",
                bound_master_node: "$MASTER_NODE"
            }
        }"#
        .replace("$MASTER_NODE", bound_master_node);
        let launch_config: LaunchConfig =
            serde_json5::from_str(&json).expect("launch config should parse");

        LaunchConfigProcessor { launch_config }
    }

    #[test]
    fn resolves_self_prefix_to_bound_master() {
        let processor = processor_with_bound_master("local_master");
        let (master_node, instance) = processor.get_subscriber_target("self:camera_rear");

        assert_eq!(master_node, "local_master");
        assert_eq!(instance, "camera_rear");
    }

    #[test]
    fn uses_bound_master_when_prefix_missing() {
        let processor = processor_with_bound_master("local_master");
        let (master_node, instance) = processor.get_subscriber_target("camera_front");

        assert_eq!(master_node, "local_master");
        assert_eq!(instance, "camera_front");
    }

    #[test]
    fn keeps_explicit_master_node() {
        let processor = processor_with_bound_master("local_master");
        let (master_node, instance) = processor.get_subscriber_target("remote_master:camera_front");

        assert_eq!(master_node, "remote_master");
        assert_eq!(instance, "camera_front");
    }

    #[test]
    fn handles_empty_master_prefix() {
        let processor = processor_with_bound_master("local_master");
        let (master_node, instance) = processor.get_subscriber_target(":camera_front");

        assert_eq!(master_node, "local_master");
        assert_eq!(instance, "camera_front");
    }
}
