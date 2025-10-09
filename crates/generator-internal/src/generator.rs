mod python;
mod rust;
pub mod types;

use crate::error::{Error, Result};
use python::PythonGenerator;
use rust::RustGenerator;
use types::{DeploymentInterface, InterfaceArtifact};
use types::{InterfaceBackend, InterfaceVariant, Language};

/// Entry point for staged interface code generation.
#[derive(Debug)]
pub struct InterfaceGenerator {
    language: Language,
    interfaces: Vec<DeploymentInterface>,
}

impl InterfaceGenerator {
    /// Starts a generation plan by selecting the target language.
    pub fn language(language: Language) -> Self {
        Self {
            language,
            interfaces: Vec::new(),
        }
    }

    /// Registers a single interface for generation.
    pub fn interface(mut self, interface: DeploymentInterface) -> Self {
        self.interfaces.push(interface);
        self
    }

    /// Registers multiple interfaces for generation.
    pub fn interfaces<I>(mut self, interfaces: I) -> Self
    where
        I: IntoIterator<Item = DeploymentInterface>,
    {
        self.interfaces.extend(interfaces);
        self
    }

    /// Generates code for every registered interface.
    pub fn build(self) -> Result<Vec<InterfaceArtifact>> {
        let generated_interfaces = self.generate();

        // TODO now generate the actual lib (structure like Cargo.toml etc...)
        generated_interfaces
    }

    fn generate(self) -> Result<Vec<InterfaceArtifact>> {
        let mut backend = Self::backend_for_language(self.language);
        for iface in &self.interfaces {
            Self::ensure_subscriber_message_format(iface)?;
            iface.register_with(backend.as_mut());
        }
        Ok(backend.finish())
    }

    fn backend_for_language(lang: Language) -> Box<dyn InterfaceBackend> {
        match lang {
            Language::Rust => Box::new(RustGenerator::new()),
            Language::Python => Box::new(PythonGenerator::new()),
        }
    }

    fn ensure_subscriber_message_format(iface: &DeploymentInterface) -> Result<()> {
        if iface.message_format().is_some() {
            return Ok(());
        }

        match iface.interface() {
            InterfaceVariant::SubscribedTopic(topic) => Err(
                Error::SubscriberTopicMessageFormatMissing(topic.name.clone()),
            ),
            InterfaceVariant::SubscribedService(service) => Err(
                Error::SubscriberServiceMessageFormatMissing(service.name.clone()),
            ),
            InterfaceVariant::SubscribedAction(action) => Err(
                Error::SubscriberActionMessageFormatMissing(action.name.clone()),
            ),
            _ => Ok(()),
        }
    }
}

// The generated code is stored inside crate::interfaces so during testing do not call this function
// directly as this would create throwaway code that will be picked up by git
// pub fn generate_interfaces_code(
//     interfaces: &[DeploymentInterface],
//     lang: Language,
// ) -> Result<Vec<String>> {
//     InterfaceGenerator::language(lang)
//         .interfaces(interfaces.iter().cloned())
//         .build()
// }
