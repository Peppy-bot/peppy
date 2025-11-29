use std::collections::HashMap;

use crate::error::{Error, Result};
use config::{NodeParameters, runtime::RuntimeConfig};

const PEPPY_RUNTIME_CONFIG: &str = "PEPPY_RUNTIME_CONFIG";

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

    pub fn get_instance_ids() -> HashMap<String, String> {
        todo!("Finish")
    }
}

#[cfg(test)]
mod tests {}
