#[cfg(test)]
mod tests;

mod actions;
mod code_builder;
mod parameters;
mod services;
mod topics;
mod type_mapping;

use super::naming::module_name_from_components;
use super::types::{InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage};
use crate::error::Result;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use std::path::Path;

/// Python-specific implementation of the interface generator.
#[derive(Default)]
pub struct PythonGenerator {
    sections: Vec<InterfaceArtifact>,
    parameters: config::NodeArguments,
}

impl PythonGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the node parameters for code generation.
    pub fn set_parameters(&mut self, parameters: config::NodeArguments) {
        self.parameters = parameters;
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }
}

impl LanguageGenerator for PythonGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) {
        PythonGenerator::push_section(self, section);
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let code = topics::build_exposed_topic(topic);
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::ExposedTopic,
            code,
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let code = services::build_exposed_service(service);
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::ExposedService,
            code,
        ));
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let code = actions::build_exposed_action(action);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: MessageFormat,
    ) -> Result<()> {
        let code = topics::build_subscribed_topic(topic, &arguments);
        let module_label = topics::subscribed_topic_module_label(topic);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedTopic,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
    ) -> Result<()> {
        let code =
            services::build_subscribed_service(service, request_arguments, response_arguments);
        let module_label = module_name_from_components(&service.node, &service.name);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedService,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: &SubscribedActionMessage,
    ) -> Result<()> {
        let code = actions::build_subscribed_action(action, messages);
        let module_label = module_name_from_components(&action.node, &action.name);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedAction,
            code,
        ));
        Ok(())
    }

    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let to_path = to_path.as_ref();
        std::fs::create_dir_all(to_path)?;

        // Generate and write parameters.py
        let parameters_code = parameters::generate_python_parameters(&self.parameters)?;
        let parameters_file = to_path.join("parameters.py");
        std::fs::write(&parameters_file, parameters_code)?;

        Ok(())
    }
}
