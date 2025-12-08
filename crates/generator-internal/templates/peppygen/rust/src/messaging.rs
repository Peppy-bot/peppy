use peppylib::runtime::Processor;
use std::path::Path;

pub struct Messenger {
    messenger: peppylib::MessengerHandle,
    runtime_processor: Processor,
}

impl Messenger {
    pub async fn connect(
        peppy_config: impl AsRef<Path>,
        host: &str,
        port: u16,
    ) -> crate::Result<Self> {
        let messenger = peppylib::MessengerHandle::from_host_port(host, port)
            .await
            .map_err(crate::Error::Messaging)?;
        let runtime_processor = Processor::new_with_peppy_config(peppy_config)?;
        Ok(Self {
            messenger,
            runtime_processor,
        })
    }

    pub fn runtime(&self) -> &Processor {
        &self.runtime_processor
    }

    pub fn handle(&self) -> &peppylib::MessengerHandle {
        &self.messenger
    }
}
