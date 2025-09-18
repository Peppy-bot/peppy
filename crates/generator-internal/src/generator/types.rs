use config::{MessageFormat, SubscribedAction, SubscribedService, SubscribedTopic};

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

// TODO: There cannot be more than one emitter with the same name in the same namespace
