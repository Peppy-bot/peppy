use crate::error::Result;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    ExposedTopic,
    ExposedService,
    ExposedAction,
    SubscribedTopic,
    SubscribedService,
    SubscribedAction,
}

#[derive(Debug, Clone)]
pub struct SubscribedActionMessage {
    pub goal_request: Option<MessageFormat>,
    pub goal_response: Option<MessageFormat>,
    pub feedback: Option<MessageFormat>,
    pub result_request: Option<MessageFormat>,
    pub result_response: Option<MessageFormat>,
}

/// Describes a concrete subscriber/exposer interface that a deployment requires.
#[derive(Debug, Clone)]
pub enum InterfaceVariant {
    ExposedTopic(ExposedTopic),
    ExposedService(ExposedService),
    ExposedAction(ExposedAction),
    SubscribedTopic(SubscribedTopic, MessageFormat),
    SubscribedService(SubscribedService, MessageFormat, MessageFormat),
    SubscribedAction(SubscribedAction, SubscribedActionMessage),
}

/// Maps a deployment interface to the message format required to bind it.
#[derive(Debug, Clone)]
pub struct DeploymentInterface {
    interface: InterfaceVariant,
}

impl DeploymentInterface {
    pub fn new(interface: InterfaceVariant) -> Self {
        Self { interface }
    }

    pub fn interface(&self) -> &InterfaceVariant {
        &self.interface
    }

    pub fn into_interface(self) -> InterfaceVariant {
        self.interface
    }
}

pub struct InterfaceArtifact {
    pub node_name: String,
    pub kind: InterfaceKind,
    pub interface: Option<InterfaceVariant>,
    pub code_output: String,
    pub submodule: Option<String>,
}

impl InterfaceArtifact {
    pub fn from_kind(node_name: &str, kind: InterfaceKind, code_output: String) -> Self {
        Self::from_kind_with_submodule(node_name, kind, code_output, None)
    }

    pub fn from_kind_with_submodule(
        node_name: &str,
        kind: InterfaceKind,
        code_output: String,
        submodule: Option<String>,
    ) -> Self {
        Self {
            node_name: node_name.to_string(),
            kind,
            interface: None,
            code_output,
            submodule,
        }
    }
}

/// Collects deployment interfaces and produces generated artifacts when finalized.
pub trait LanguageGenerator {
    fn push_section(&mut self, section: InterfaceArtifact);
    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()>;
    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()>;
    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()>;
    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: MessageFormat,
    ) -> Result<()>;
    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
    ) -> Result<()>;
    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: &SubscribedActionMessage,
    ) -> Result<()>;
    /// Finalizes the builder and return a path to the library
    fn build(self, to_path: impl AsRef<Path>) -> Result<()>;
}

impl DeploymentInterface {
    pub fn register_with<B: LanguageGenerator + ?Sized>(&self, backend: &mut B) -> Result<()> {
        match self.interface() {
            InterfaceVariant::ExposedTopic(topic) => backend.add_exposed_topic(topic),
            InterfaceVariant::ExposedService(service) => backend.add_exposed_service(service),
            InterfaceVariant::ExposedAction(action) => backend.add_exposed_action(action),
            InterfaceVariant::SubscribedTopic(topic, format) => {
                backend.add_subscribed_topic(topic, format.clone())
            }
            InterfaceVariant::SubscribedService(service, request_arguments, response_arguments) => {
                backend.add_subscribed_service(service, request_arguments, response_arguments)
            }
            InterfaceVariant::SubscribedAction(action, messages) => {
                backend.add_subscribed_action(action, messages)
            }
        }
    }
}

#[derive(Clone)]
pub struct CapnpSchema {
    file_stem: String,
    schema: String,
}

impl CapnpSchema {
    pub fn new(file_stem: String, schema: String) -> Self {
        Self { file_stem, schema }
    }

    pub fn file_stem(&self) -> &str {
        &self.file_stem
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}
