use config::NodeConfig;

use crate::Result;

pub struct InterfacesGenerator {}

impl InterfacesGenerator {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    pub fn generate_interfaces(config: NodeConfig) {}
}
