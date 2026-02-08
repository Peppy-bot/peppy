#[cfg(test)]
mod tests;

use super::naming::{non_empty_str, prefixed_name};
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

    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }
}

impl LanguageGenerator for PythonGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) {
        PythonGenerator::push_section(self, section);
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let name = prefixed_name("exposed_topic", non_empty_str(topic.name.as_str()), "topic");
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::ExposedTopic,
            format!("def {name}():\n    raise NotImplementedError(\"publish PMI topic\")\n"),
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let name = prefixed_name(
            "exposed_service",
            non_empty_str(service.name.as_str()),
            "service",
        );
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::ExposedService,
            format!("def {name}():\n    raise NotImplementedError(\"expose PMI service\")\n"),
        ));
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let name = prefixed_name("exposed_action", non_empty_str(&action.name), "action");
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            format!("def {name}():\n    raise NotImplementedError(\"expose PMI action\")\n"),
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        _arguments: MessageFormat,
    ) -> Result<()> {
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::SubscribedTopic,
            "async def on_message():\n    raise NotImplementedError(\"await for message with PMI\")\n"
                .to_string(),
        ));
        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        _request_arguments: &MessageFormat,
        _response_arguments: &MessageFormat,
    ) -> Result<()> {
        let fn_name = prefixed_name("on", non_empty_str(service.name.as_str()), "service");
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::SubscribedService,
            format!(
                "async def {}():\n    raise NotImplementedError(\"await for service response with PMI\")\n",
                fn_name
            ),
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        _messages: &SubscribedActionMessage,
    ) -> Result<()> {
        let base_name = prefixed_name("on", non_empty_str(action.name.as_str()), "action");
        let mut sections = Vec::new();

        sections.push(format!(
            "async def {}_feedback():\n    raise NotImplementedError(\"await for action feedback with PMI\")\n",
            base_name
        ));
        sections.push(format!(
            "async def {}_result():\n    raise NotImplementedError(\"await for action result with PMI\")\n",
            base_name
        ));

        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::SubscribedAction,
            sections.join("\n"),
        ));
        Ok(())
    }
    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let _ = to_path;
        let _artifacts = self.into_artifacts();
        // TODO: implement Python project scaffold generation.
        Ok(())
    }
}
