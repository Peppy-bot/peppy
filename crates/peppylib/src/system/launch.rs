use crate::error::{Error, Result};
use config::{NodeParameters, launch_config::RuntimeConfig};

const PEPPY_RUNTIME_CONFIG: &str = "PEPPY_RUNTIME_CONFIG";

pub struct TargetInstance {
    pub master_node: String,
    pub instance_id: String,
}

pub struct RuntimeProcessor {
    launch_config: RuntimeConfig,
}

impl RuntimeProcessor {
    pub fn new() -> Result<Self> {
        let launch_config_path = std::env::var(PEPPY_RUNTIME_CONFIG).map_err(|source| {
            Error::MissingInstanceIdEnvVar {
                var: PEPPY_RUNTIME_CONFIG,
                source,
            }
        })?;
        let launch_config = RuntimeProcessor::get_peppy_deployment_config(&launch_config_path)?;
        Ok(Self { launch_config })
    }

    fn get_peppy_deployment_config(launch_config_path: &str) -> Result<RuntimeConfig> {
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

    pub fn current_instance_id(&self) -> &str {
        self.launch_config.deployment_instance.instance_id.as_str()
    }

    pub fn input_parameters(&self) -> &NodeParameters {
        &self.launch_config.deployment_instance.parameters
    }

    pub fn node_name(&self) -> &str {
        self.launch_config.node_name.as_str()
    }

    pub fn bound_master_node(&self) -> &str {
        self.launch_config.bound_master_node.as_str()
    }

    /// Given a `subscriber_id` in the form of `<master_node>:<node_instance>` (example `self:the_node`),
    /// returns a list of target instances (`master_node` + `instance_id`).
    ///  - If `self` is given, it's replaced by the current bound master node
    ///  - If nothing is given for the master_node, the current bound master node is used
    ///  - If a matching `subscriber_target` is configured in the runtime config, return all of its target instances
    ///  - Otherwise, treat the provided `subscriber_id` as the target instance tag
    pub fn get_subscriber_targets(&self, subscriber_id: &str) -> Vec<TargetInstance> {
        let bound_master_node = self.launch_config.bound_master_node.as_str();

        let target_instance_tags = self
            .launch_config
            .deployment_instance
            .subscriber_targets
            .iter()
            .find(|target| target.id.as_str() == subscriber_id)
            .map(|target| target.target_instance_ids.as_slice());

        match target_instance_tags {
            Some(tags) if !tags.is_empty() => tags
                .iter()
                .map(|tag| Self::resolve_target_instance(bound_master_node, tag))
                .collect(),
            _ => vec![Self::resolve_target_instance(
                bound_master_node,
                subscriber_id,
            )],
        }
    }

    fn resolve_target_instance(
        bound_master_node: &str,
        target_instance_tag: &str,
    ) -> TargetInstance {
        match target_instance_tag.split_once(':') {
            Some((master_node, node_instance)) => {
                let resolved_master = match master_node {
                    "" | "self" => bound_master_node,
                    other => other,
                };
                TargetInstance {
                    master_node: resolved_master.to_string(),
                    instance_id: node_instance.to_string(),
                }
            }
            None => TargetInstance {
                master_node: bound_master_node.to_string(),
                instance_id: target_instance_tag.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor_with_bound_master(bound_master_node: &str) -> RuntimeProcessor {
        // A launch configuration carries the deployment instance plus runtime identifiers
        let json = format!(
            r#"{{
                deployment_instance: {{
                    instance_id: "instance_id"
                }},
                node_name: "node_name",
                bound_master_node: "{bound_master_node}"
            }}"#
        );
        let launch_config: RuntimeConfig =
            serde_json5::from_str(&json).expect("launch config should parse");

        RuntimeProcessor { launch_config }
    }

    fn processor_with_subscriber_targets(bound_master_node: &str) -> RuntimeProcessor {
        let json = format!(
            r#"{{
                deployment_instance: {{
                    instance_id: "instance_id",
                    subscriber_targets: [
                        {{
                            id: "camera_stream",
                            target_instance_ids: [
                                "camera_front",
                                "self:camera_rear",
                                ":camera_side",
                                "remote_master:camera_top"
                            ]
                        }}
                    ]
                }},
                node_name: "node_name",
                bound_master_node: "{bound_master_node}"
            }}"#
        );
        let launch_config: RuntimeConfig =
            serde_json5::from_str(&json).expect("launch config should parse");

        RuntimeProcessor { launch_config }
    }

    #[test]
    fn resolves_self_prefix_to_bound_master() {
        let processor = processor_with_bound_master("local_master");
        let target = processor
            .get_subscriber_targets("self:camera_rear")
            .pop()
            .expect("target should exist");

        assert_eq!(target.master_node, "local_master");
        assert_eq!(target.instance_id, "camera_rear");
    }

    #[test]
    fn uses_bound_master_when_prefix_missing() {
        let processor = processor_with_bound_master("local_master");
        let target = processor
            .get_subscriber_targets("camera_front")
            .pop()
            .expect("target should exist");

        assert_eq!(target.master_node, "local_master");
        assert_eq!(target.instance_id, "camera_front");
    }

    #[test]
    fn keeps_explicit_master_node() {
        let processor = processor_with_bound_master("local_master");
        let target = processor
            .get_subscriber_targets("remote_master:camera_front")
            .pop()
            .expect("target should exist");

        assert_eq!(target.master_node, "remote_master");
        assert_eq!(target.instance_id, "camera_front");
    }

    #[test]
    fn handles_empty_master_prefix() {
        let processor = processor_with_bound_master("local_master");
        let target = processor
            .get_subscriber_targets(":camera_front")
            .pop()
            .expect("target should exist");

        assert_eq!(target.master_node, "local_master");
        assert_eq!(target.instance_id, "camera_front");
    }

    #[test]
    fn resolves_configured_subscriber_targets() {
        let processor = processor_with_subscriber_targets("local_master");
        let targets = processor.get_subscriber_targets("camera_stream");

        assert_eq!(targets.len(), 4);
        assert_eq!(targets[0].master_node, "local_master");
        assert_eq!(targets[0].instance_id, "camera_front");
        assert_eq!(targets[1].master_node, "local_master");
        assert_eq!(targets[1].instance_id, "camera_rear");
        assert_eq!(targets[2].master_node, "local_master");
        assert_eq!(targets[2].instance_id, "camera_side");
        assert_eq!(targets[3].master_node, "remote_master");
        assert_eq!(targets[3].instance_id, "camera_top");
    }
}
