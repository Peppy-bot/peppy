mod builder;
mod python;
mod rust;

use config::{Language, NodeConfig};

use crate::Result;
use builder::generate_interfaces as compose_interfaces;
use python::PythonGenerator;
use rust::RustGenerator;

pub struct InterfacesGenerator {}

impl InterfacesGenerator {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    /// Called everytime a new change to a peppy configuration is detected
    pub fn generate_interfaces(config: &NodeConfig) -> Vec<String> {
        let lang = config.manifest.language;
        let generator: Box<dyn builder::InterfaceGenerator> = match lang {
            Language::Rust => Box::new(RustGenerator::new()),
            Language::Python => Box::new(PythonGenerator::new()),
        };

        compose_interfaces(generator.as_ref(), &config.interfaces)
    }
}
