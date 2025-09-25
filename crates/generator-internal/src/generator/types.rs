use config::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Language {
    Python,
    #[default]
    Rust,
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

    pub fn subscriber(&self) -> &T {
        &self.subscriber
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
