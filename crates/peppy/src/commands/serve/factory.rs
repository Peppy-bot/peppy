use super::{
    messaging::{MessengerBackend, MockAdapter, ZenohAdapter},
    types::{Engine, MessagingConfiguration},
};

pub struct MessagingFactory {}

impl MessagingFactory {
    pub fn build_messenger(configuration: MessagingConfiguration) -> Box<dyn MessengerBackend> {
        match configuration.engine {
            Engine::Zenoh => Box::new(ZenohAdapter::new(configuration.host, configuration.port)),
            Engine::Mock => Box::new(MockAdapter::default()),
        }
    }
}
