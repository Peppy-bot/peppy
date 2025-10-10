use crate::error::Result;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Language {
    Python,
    #[default]
    Rust,
}

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
    pub goal: MessageFormat,
    pub feedback: MessageFormat,
    pub result: MessageFormat,
}

/// Describes a concrete subscriber/exposer interface that a deployment requires.
#[derive(Debug, Clone)]
pub enum InterfaceVariant {
    ExposedTopic(ExposedTopic),
    ExposedService(ExposedService),
    ExposedAction(ExposedAction),
    SubscribedTopic(SubscribedTopic, MessageFormat),
    SubscribedService(SubscribedService, MessageFormat),
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

    // pub fn message_format(&self) -> Option<&MessageFormat> {
    //     match self.interface() {
    //         InterfaceVariant::SubscribedTopic(_, format)
    //         | InterfaceVariant::SubscribedService(_, format) => Some(format),
    //         _ => None,
    //     }
    // }

    // pub fn into_message_format(self) -> Option<MessageFormat> {
    //     match self.interface {
    //         InterfaceVariant::SubscribedTopic(_, format)
    //         | InterfaceVariant::SubscribedService(_, format) => Some(format),
    //         _ => None,
    //     }
    // }

    // pub fn action_messages(&self) -> Option<&SubscribedActionMessage> {
    //     match self.interface() {
    //         InterfaceVariant::SubscribedAction(_, messages) => Some(messages),
    //         _ => None,
    //     }
    // }

    // pub fn into_action_messages(self) -> Option<SubscribedActionMessage> {
    //     match self.interface {
    //         InterfaceVariant::SubscribedAction(_, messages) => Some(messages),
    //         _ => None,
    //     }
    // }
}

pub struct InterfaceArtifact {
    pub kind: InterfaceKind,
    pub interface: Option<InterfaceVariant>,
    pub code_output: String,
}

impl InterfaceArtifact {
    pub fn from_kind(kind: InterfaceKind, code_output: String) -> Self {
        Self {
            kind,
            interface: None,
            code_output,
        }
    }
}

/// Collects deployment interfaces and produces generated artifacts when finalized.
pub trait InterfaceBackend {
    fn push_section(&mut self, section: InterfaceArtifact);
    fn add_exposed_topic(&mut self, topic: &ExposedTopic);
    fn add_exposed_service(&mut self, service: &ExposedService);
    fn add_exposed_action(&mut self, action: &ExposedAction);
    fn add_subscribed_topic(&mut self, topic: &SubscribedTopic, arguments: Option<&MessageFormat>);
    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    );
    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        arguments: Option<&SubscribedActionMessage>,
    );
    /// Finalizes the builder and return a path to the library
    fn build(self, to_path: impl AsRef<Path>) -> Result<()>;
}

impl DeploymentInterface {
    pub fn register_with<B: InterfaceBackend + ?Sized>(&self, backend: &mut B) {
        match self.interface() {
            InterfaceVariant::ExposedTopic(topic) => backend.add_exposed_topic(topic),
            InterfaceVariant::ExposedService(service) => backend.add_exposed_service(service),
            InterfaceVariant::ExposedAction(action) => backend.add_exposed_action(action),
            InterfaceVariant::SubscribedTopic(topic, format) => {
                backend.add_subscribed_topic(topic, Some(format))
            }
            InterfaceVariant::SubscribedService(service, format) => {
                backend.add_subscribed_service(service, Some(format))
            }
            InterfaceVariant::SubscribedAction(action, messages) => {
                backend.add_subscribed_action(action, Some(messages))
            }
        }
    }
}
