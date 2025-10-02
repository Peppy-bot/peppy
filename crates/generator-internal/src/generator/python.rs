use crate::generator::types::SubscriberMap;

use super::types::InterfaceGenerator;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
    SubscribedTopic,
};

/// Python-specific implementation of the interface generator.
pub struct PythonGenerator;

impl PythonGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl InterfaceGenerator for PythonGenerator {
    fn gen_subscribed_topics(&self, topics: &[SubscriberMap<SubscribedTopic>]) -> String {
        let mut out = String::new();
        for (i, _t) in topics.iter().enumerate() {
            out.push_str(&format!("def subscribed_topic_{}():\n    pass\n", i));
        }
        out
    }

    fn gen_subscribed_services(&self, services: &[SubscriberMap<SubscribedService>]) -> String {
        let mut out = String::new();
        for (i, _s) in services.iter().enumerate() {
            out.push_str(&format!("def subscribed_service_{}():\n    pass\n", i));
        }
        out
    }

    fn gen_subscribed_actions(&self, actions: &[SubscriberMap<SubscribedAction>]) -> String {
        let mut out = String::new();
        for (i, _a) in actions.iter().enumerate() {
            out.push_str(&format!("def subscribed_action_{}():\n    pass\n", i));
        }
        out
    }

    fn gen_exposed_topics(&self, topics: &[ExposedTopic]) -> String {
        let mut out = String::new();
        for (i, _t) in topics.iter().enumerate() {
            out.push_str(&format!("def exposed_topic_{}():\n    pass\n", i));
        }
        out
    }

    fn gen_exposed_services(&self, services: &[ExposedService]) -> String {
        let mut out = String::new();
        for (i, _s) in services.iter().enumerate() {
            out.push_str(&format!("def exposed_service_{}():\n    pass\n", i));
        }
        out
    }

    fn gen_exposed_actions(&self, actions: &[ExposedAction]) -> String {
        let mut out = String::new();
        for (i, _a) in actions.iter().enumerate() {
            out.push_str(&format!("def exposed_action_{}():\n    pass\n", i));
        }
        out
    }
}
