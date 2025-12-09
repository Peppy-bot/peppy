use crate::error::Result;
use config::{peppy_config::DeploymentInstance, runtime::RuntimeConfig};

/// This struct goal is to generate the RuntimeConfig passed to every node at runtime through consts::PEPPY_RUNTIME_CONFIG
pub struct Generator {
    runtime_config: RuntimeConfig,
}

impl Generator {
    pub fn new(
        node_name: &str,
        bound_master_node: &str,
        deployment_instance: DeploymentInstance,
        codegen_peppy_config_md5: &str,
    ) -> Result<Self> {
        let runtime_config = RuntimeConfig::new(
            deployment_instance,
            node_name,
            bound_master_node,
            codegen_peppy_config_md5,
        )?;
        Ok(Self { runtime_config })
    }

    pub fn runtime_config(&self) -> &RuntimeConfig {
        &self.runtime_config
    }
}

#[cfg(test)]
mod tests {}
