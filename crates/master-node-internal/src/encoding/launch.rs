use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::launch_capnp;
use crate::names;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub peppy_launch_json5: String,
    pub nodes_directory: PathBuf,
    pub launch_runtime_config_json5: String,
}

impl LaunchRequest {
    pub fn new(
        peppy_launch_json5: impl Into<String>,
        nodes_directory: impl Into<PathBuf>,
        launch_runtime_config_json5: impl Into<String>,
    ) -> Self {
        Self {
            peppy_launch_json5: peppy_launch_json5.into(),
            nodes_directory: nodes_directory.into(),
            launch_runtime_config_json5: launch_runtime_config_json5.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<launch_capnp::launch_request::Builder>();
            request.set_peppy_launch_json5(&self.peppy_launch_json5);
            request.set_nodes_directory(self.nodes_directory.to_string_lossy());
            request.set_launch_runtime_config_json5(&self.launch_runtime_config_json5);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<launch_capnp::launch_request::Reader>()?;
        Ok(Self {
            peppy_launch_json5: request.get_peppy_launch_json5()?.to_str()?.to_owned(),
            nodes_directory: PathBuf::from(request.get_nodes_directory()?.to_str()?),
            launch_runtime_config_json5: request
                .get_launch_runtime_config_json5()?
                .to_str()?
                .to_owned(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<LaunchResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            names::STACK_LAUNCH,
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        LaunchResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResponse {
    pub success: bool,
    pub error_message: String,
}

impl LaunchResponse {
    pub fn new() -> Self {
        Self {
            success: true,
            error_message: String::new(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<launch_capnp::launch_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<launch_capnp::launch_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

impl Default for LaunchResponse {
    fn default() -> Self {
        Self::new()
    }
}
