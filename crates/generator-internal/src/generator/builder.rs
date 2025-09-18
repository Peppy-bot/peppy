use config::{
    ExposedAction, ExposedService, ExposedTopic, Interfaces, SubscribedAction, SubscribedService,
    SubscribedTopic,
};
use std::collections::HashMap;

use config::MessageFormat;

use crate::error::{Error, Result};

use crate::generator::types::AllowedSubscriber;
use crate::generator::types::SubscriberMap;

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

trait HasName {
    fn name(&self) -> &str;
}

impl HasName for SubscribedTopic {
    fn name(&self) -> &str {
        &self.name
    }
}

impl HasName for SubscribedService {
    fn name(&self) -> &str {
        &self.name
    }
}

impl HasName for SubscribedAction {
    fn name(&self) -> &str {
        &self.name
    }
}

trait ExposedWithFormat {
    fn name(&self) -> Option<&str>;
    fn message_format(&self) -> Option<&MessageFormat>;
}

impl ExposedWithFormat for ExposedTopic {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn message_format(&self) -> Option<&MessageFormat> {
        self.message_format.as_ref()
    }
}

impl ExposedWithFormat for ExposedService {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn message_format(&self) -> Option<&MessageFormat> {
        self.message_format.as_ref()
    }
}

pub fn compose_interfaces(
    generator: &dyn InterfaceGenerator,
    interfaces: &Interfaces,
) -> Result<Vec<String>> {
    let exposed_topics = interfaces
        .exposes
        .as_ref()
        .and_then(|exposes| exposes.topics.as_deref())
        .unwrap_or(&[]);
    let exposed_services = interfaces
        .exposes
        .as_ref()
        .and_then(|exposes| exposes.services.as_deref())
        .unwrap_or(&[]);
    let exposed_actions = interfaces
        .exposes
        .as_ref()
        .and_then(|exposes| exposes.actions.as_deref())
        .unwrap_or(&[]);

    let topic_formats = build_format_index(exposed_topics);
    let service_formats = build_format_index(exposed_services);

    let subscribed_topics = interfaces
        .subscribes_to
        .as_ref()
        .and_then(|subs| subs.topics.as_deref())
        .unwrap_or(&[]);
    let subscribed_services = interfaces
        .subscribes_to
        .as_ref()
        .and_then(|subs| subs.services.as_deref())
        .unwrap_or(&[]);
    let subscribed_actions = interfaces
        .subscribes_to
        .as_ref()
        .and_then(|subs| subs.actions.as_deref())
        .unwrap_or(&[]);

    let mapped_topics = map_subscribers_to_messages_format(
        subscribed_topics,
        &topic_formats,
        Error::SubscriberTopicMessageFormatMissing,
    )?;
    let mapped_services = map_subscribers_to_messages_format(
        subscribed_services,
        &service_formats,
        Error::SubscriberServiceMessageFormatMissing,
    )?;
    let mapped_actions = map_subscribers_to_messages_format(
        subscribed_actions,
        &topic_formats,
        Error::SubscriberActionMessageFormatMissing,
    )?;

    let mut out = Vec::with_capacity(6);
    out.push(generator.gen_subscribed_topics(&mapped_topics));
    out.push(generator.gen_subscribed_services(&mapped_services));
    out.push(generator.gen_subscribed_actions(&mapped_actions));
    out.push(generator.gen_exposed_topics(exposed_topics));
    out.push(generator.gen_exposed_services(exposed_services));
    out.push(generator.gen_exposed_actions(exposed_actions));

    Ok(out)
}

fn build_format_index(exposed: &[impl ExposedWithFormat]) -> HashMap<String, MessageFormat> {
    exposed
        .iter()
        .filter_map(|item| {
            item.name()
                .zip(item.message_format().cloned())
                .map(|(name, format)| (name.to_owned(), format))
        })
        .collect()
}

fn map_subscribers_to_messages_format<T, F>(
    subscribers: &[T],
    formats: &HashMap<String, MessageFormat>,
    missing_err: F,
) -> Result<Vec<SubscriberMap<T>>>
where
    T: AllowedSubscriber + Clone + HasName,
    F: Fn(String) -> Error,
{
    // TODO: Double check, this is generated code and might be wrong
    // subscribers
    //     .iter()
    //     .cloned()
    //     .map(|subscriber| {
    //         let format = formats
    //             .get(subscriber.name())
    //             .cloned()
    //             .ok_or_else(|| missing_err(subscriber.name().to_owned()))?;
    //         Ok(SubscriberMap::new(subscriber, format))
    //     })
    //     .collect()
    todo!("Finish")
}
