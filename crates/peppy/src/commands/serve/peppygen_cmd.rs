use super::{ServeAsyncCommand, ServeFuture};
use crate::{AppContext, AppEvent, Result};
use config::{Interfaces, Language};
use peppycl::generate_interfaces_code;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct InterfacesGenerator {
    interfaces: Arc<Interfaces>,
    for_language: Language,
    event_subscriber: broadcast::Receiver<AppEvent>,
}

impl InterfacesGenerator {
    pub fn new(ctx: &AppContext, interfaces: &Interfaces, for_language: &Language) -> Result<Self> {
        let event_subscriber = ctx.subscribe();
        Ok(Self {
            event_subscriber,
            interfaces: Arc::new(interfaces.clone()),
            for_language: *for_language,
        })
    }
}

impl ServeAsyncCommand for InterfacesGenerator {
    fn run(&self) -> ServeFuture {
        let interfaces = Arc::clone(&self.interfaces);
        let for_language = self.for_language;
        Box::pin(async move {
            let _gen = generate_interfaces_code(interfaces.as_ref(), &for_language);
            Ok(())
        })
    }
}
