use super::python;
use super::rust;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Language {
    Python,
    #[default]
    Rust,
}

/// Describes a concrete subscriber/exposer interface that a deployment requires.
#[derive(Debug, Clone)]
pub enum InterfaceVariant {
    ExposedTopic(ExposedTopic),
    ExposedService(ExposedService),
    ExposedAction(ExposedAction),
    SubscribedTopic(SubscribedTopic),
    SubscribedService(SubscribedService),
    SubscribedAction(SubscribedAction),
}

/// Maps a deployment interface to the message format required to bind it.
#[derive(Debug, Clone)]
pub struct DeploymentInterface {
    interface: InterfaceVariant,
    message_format: Option<MessageFormat>,
}

impl DeploymentInterface {
    pub fn new(interface: InterfaceVariant, message_format: Option<MessageFormat>) -> Self {
        Self {
            interface,
            message_format,
        }
    }

    pub fn interface(&self) -> &InterfaceVariant {
        &self.interface
    }

    pub fn into_interface(self) -> InterfaceVariant {
        self.interface
    }

    pub fn message_format(&self) -> Option<&MessageFormat> {
        self.message_format.as_ref()
    }

    pub fn into_message_format(self) -> Option<MessageFormat> {
        self.message_format
    }
}

pub trait DeploymentInterfaceGenerator {
    fn gen_interface(&self, iface: &DeploymentInterface, fmt: Option<&MessageFormat>) -> String;
}

pub trait InterfaceBackend {
    fn exposed_topic(&self, topic: &ExposedTopic) -> String;
    fn exposed_service(&self, service: &ExposedService) -> String;
    fn exposed_action(&self, action: &ExposedAction) -> String;
    fn subscribed_topic(
        &self,
        topic: &SubscribedTopic,
        arguments: Option<&MessageFormat>,
    ) -> String;
    fn subscribed_service(
        &self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    ) -> String;
    fn subscribed_action(
        &self,
        action: &SubscribedAction,
        arguments: Option<&MessageFormat>,
    ) -> String;
}

impl DeploymentInterface {
    pub fn render_with<B: InterfaceBackend + ?Sized>(&self, backend: &B) -> String {
        let arguments = self.message_format();
        match self.interface() {
            InterfaceVariant::ExposedTopic(topic) => backend.exposed_topic(topic),
            InterfaceVariant::ExposedService(service) => backend.exposed_service(service),
            InterfaceVariant::ExposedAction(action) => backend.exposed_action(action),
            InterfaceVariant::SubscribedTopic(topic) => backend.subscribed_topic(topic, arguments),
            InterfaceVariant::SubscribedService(service) => {
                backend.subscribed_service(service, arguments)
            }
            InterfaceVariant::SubscribedAction(action) => {
                backend.subscribed_action(action, arguments)
            }
        }
    }
}
