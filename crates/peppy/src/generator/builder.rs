use config::{
    ExposedAction, ExposedService, ExposedTopic, Interfaces, SubscribedAction, SubscribedService,
    SubscribedTopic,
};

/// Language-agnostic generator interface. Each method is pure: input -> output string.
pub trait InterfaceGenerator {
    // Subscribes to
    fn gen_subscribed_topics(&self, topics: &[SubscribedTopic]) -> String;
    fn gen_subscribed_services(&self, services: &[SubscribedService]) -> String;
    fn gen_subscribed_actions(&self, actions: &[SubscribedAction]) -> String;

    // Exposes
    fn gen_exposed_topics(&self, topics: &[ExposedTopic]) -> String;
    fn gen_exposed_services(&self, services: &[ExposedService]) -> String;
    fn gen_exposed_actions(&self, actions: &[ExposedAction]) -> String;
}

/// Compose the full interface generation from the language-specific generator.
pub fn generate_interfaces(
    generator: &dyn InterfaceGenerator,
    interfaces: &Interfaces,
) -> Vec<String> {
    let mut out: Vec<String> = vec![];

    if let Some(sub) = &interfaces.subscribes_to {
        if let Some(v) = &sub.topics {
            out.push(generator.gen_subscribed_topics(v));
        }
        if let Some(v) = &sub.services {
            out.push(generator.gen_subscribed_services(v));
        }
        if let Some(v) = &sub.actions {
            out.push(generator.gen_subscribed_actions(v));
        }
    }

    if let Some(exp) = &interfaces.exposes {
        if let Some(v) = &exp.topics {
            out.push(generator.gen_exposed_topics(v));
        }
        if let Some(v) = &exp.services {
            out.push(generator.gen_exposed_services(v));
        }
        if let Some(v) = &exp.actions {
            out.push(generator.gen_exposed_actions(v));
        }
    }

    out
}
