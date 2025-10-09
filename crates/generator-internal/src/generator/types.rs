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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    ExposedTopic,
    ExposedService,
    ExposedAction,
    SubscribedTopic,
    SubscribedService,
    SubscribedAction,
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

/// Collects deployment interfaces and produces generated artifacts when finalized.
pub trait InterfaceBackend {
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
        arguments: Option<&MessageFormat>,
    );
    /// Finalizes the builder, yielding all generated artifacts.
    fn finish(self: Box<Self>) -> Vec<InterfaceArtifact>;
}

impl DeploymentInterface {
    pub fn register_with<B: InterfaceBackend + ?Sized>(&self, backend: &mut B) {
        let arguments = self.message_format();
        match self.interface() {
            InterfaceVariant::ExposedTopic(topic) => backend.add_exposed_topic(topic),
            InterfaceVariant::ExposedService(service) => backend.add_exposed_service(service),
            InterfaceVariant::ExposedAction(action) => backend.add_exposed_action(action),
            InterfaceVariant::SubscribedTopic(topic) => {
                backend.add_subscribed_topic(topic, arguments)
            }
            InterfaceVariant::SubscribedService(service) => {
                backend.add_subscribed_service(service, arguments)
            }
            InterfaceVariant::SubscribedAction(action) => {
                backend.add_subscribed_action(action, arguments)
            }
        }
    }
}
