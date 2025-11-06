pub struct Messenger {
    messenger: peppylib::MessengerHandle,
    namespace: String,
}

impl Messenger {
    pub async fn connect(host: &str, port: u16) -> crate::Result<Self> {
        let messenger = peppylib::MessengerHandle::from_host_port(host, port)
            .await
            .map_err(crate::Error::Messaging)?;
        let namespace = std::env::var("PEPPY_NAMESPACE")
            .map(|value| {
                if value.trim().is_empty() {
                    "/".to_string()
                } else {
                    value
                }
            })
            .unwrap_or_else(|_| "/".to_string());
        Ok(Self {
            messenger,
            namespace,
        })
    }

    pub fn namespace(&self) -> &str {
        self.namespace.as_str()
    }

    pub fn handle(&self) -> &peppylib::MessengerHandle {
        &self.messenger
    }
}
