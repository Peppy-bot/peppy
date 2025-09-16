use super::{ServeAsyncCommand, ServeFuture};
use crate::Result;
use config::{Interfaces, Language};
use peppycl::generate_interfaces_code;

pub struct InterfacesGenerator {
    interfaces: Interfaces,
    for_language: Language,
}

impl InterfacesGenerator {
    pub fn new(interfaces: &Interfaces, for_language: &Language) -> Result<Self> {
        Ok(Self {
            interfaces: interfaces.clone(),
            for_language: *for_language,
        })
    }
}

impl ServeAsyncCommand for InterfacesGenerator {
    fn run(&self) -> ServeFuture {
        let interfaces = self.interfaces.clone();
        let for_language = self.for_language;
        Box::pin(async move {
            let _gen = generate_interfaces_code(&interfaces, &for_language);
            Ok(())
        })
    }
}
