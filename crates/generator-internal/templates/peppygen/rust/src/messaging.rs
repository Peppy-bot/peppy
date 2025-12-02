use peppylib::system::RuntimeProcessor;

pub struct Messenger {
    messenger: peppylib::MessengerHandle,
    runtime_processor: RuntimeProcessor,
}

impl Messenger {
    pub async fn connect(host: &str, port: u16) -> crate::Result<Self> {
        let messenger = peppylib::MessengerHandle::from_host_port(host, port)
            .await
            .map_err(crate::Error::Messaging)?;
        let runtime_processor = RuntimeProcessor::new()?;
        Ok(Self { messenger, runtime_processor })
    }

    pub fn runtime(&self) -> &RuntimeProcessor {
        &self.runtime_processor
    }

    pub fn handle(&self) -> &peppylib::MessengerHandle {
        &self.messenger
    }
}
