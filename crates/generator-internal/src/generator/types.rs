use config::{
    Deployment, DeploymentSource, ExposedAction, ExposedService, ExposedTopic, MessageFormat,
    NodeConfig, SubscribedAction, SubscribedService, SubscribedTopic,
};

#[derive(Debug, Clone)]
pub struct NodeSource {
    source: DeploymentSource,
    node: NodeConfig,
}

impl NodeSource {
    pub fn new(source: DeploymentSource, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> &DeploymentSource {
        &self.source
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }
}

#[derive(Debug, Clone)]
pub struct DeploymentMap {
    deployment: Deployment,
    node: NodeSource,
}

impl DeploymentMap {
    pub fn new(deployment: Deployment, node: NodeSource) -> Self {
        Self { deployment, node }
    }

    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    pub fn node_source(&self) -> &NodeSource {
        &self.node
    }
}

pub trait AllowedSubscriber {}

impl AllowedSubscriber for SubscribedTopic {}
impl AllowedSubscriber for SubscribedService {}
impl AllowedSubscriber for SubscribedAction {}

// A subscribed topic has a name but the type of messages the subscriber subscribes to is determined
// by actually finding the associated node that emit this message. The following struct exists to map
// a subscriber to its associated emitter
pub struct SubscriberMap<T: AllowedSubscriber> {
    subscriber: T,
    message_format: MessageFormat,
}

impl<T: AllowedSubscriber> SubscriberMap<T> {
    pub fn new(subscriber: T, message_format: MessageFormat) -> Self {
        Self {
            subscriber,
            message_format,
        }
    }
}

/// Language-agnostic generator interface. Each method is pure: input -> output string.
pub trait InterfaceGenerator {
    // Subscribes to
    fn gen_subscribed_topics(&self, topics: &[SubscriberMap<SubscribedTopic>]) -> String;
    fn gen_subscribed_services(&self, services: &[SubscriberMap<SubscribedService>]) -> String;
    fn gen_subscribed_actions(&self, actions: &[SubscriberMap<SubscribedAction>]) -> String;

    // Exposes
    fn gen_exposed_topics(&self, topics: &[ExposedTopic]) -> String;
    fn gen_exposed_services(&self, services: &[ExposedService]) -> String;
    fn gen_exposed_actions(&self, actions: &[ExposedAction]) -> String;
}
