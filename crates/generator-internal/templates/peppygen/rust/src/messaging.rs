pub struct Messenger {
    messenger: peppylib::MessengerHandle,
    node_name: String,
}

impl Messenger {
    pub async fn connect(host: &str, port: u16) -> crate::Result<Self> {
        let messenger = peppylib::MessengerHandle::from_host_port(host, port)
            .await
            .map_err(crate::Error::Messaging)?;
        let node_name = std::env::var("PEPPY_NODE_NAME").map_err(|source| {
            crate::Error::MissingInstanceIdEnvVar {
                var: "PEPPY_NODE_NAME",
                source,
            }
        })?;

        // TODO: Maybe use peppy::commands::node::NodeName?
        if !node_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(crate::Error::InvalidNodeName { node_name });
        }
        Ok(Self {
            messenger,
            node_name,
        })
    }

    pub fn node_name(&self) -> &str {
        self.node_name.as_str()
    }

    pub fn handle(&self) -> &peppylib::MessengerHandle {
        &self.messenger
    }
}
