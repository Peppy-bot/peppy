use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStopRequest {
    pub instance_id: String,
}

impl NodeStopRequest {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_stop_request::Builder>();
            request.set_instance_id(&self.instance_id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_stop_request::Reader>()?;
        Ok(Self {
            instance_id: request.get_instance_id()?.to_str()?.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStopResponse {
    pub success: bool,
    pub error_message: Option<String>,
    /// `true` when the node had to be force-killed because it did not exit
    /// within the cooperative shutdown grace period. `false` when it exited
    /// gracefully (or on any failure response).
    pub force_killed: bool,
}

impl NodeStopResponse {
    pub fn new(success: bool, error_message: Option<String>) -> Self {
        Self {
            success,
            error_message,
            force_killed: false,
        }
    }

    /// Success after the node exited gracefully within the grace period.
    pub fn success() -> Self {
        Self::new(true, None)
    }

    /// Success, but the node ignored the cooperative shutdown and had to be
    /// force-killed (SIGKILL to its process group).
    pub fn success_force_killed() -> Self {
        Self {
            success: true,
            error_message: None,
            force_killed: true,
        }
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_stop_response::Builder>();
            response.set_success(self.success);
            response.set_force_killed(self.force_killed);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_stop_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
            force_killed: response.get_force_killed(),
        })
    }
}
