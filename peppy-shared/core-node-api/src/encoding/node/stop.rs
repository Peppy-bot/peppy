use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message};

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

/// The response of a node stop that succeeded. A failed stop is an error
/// reply, with a failure kind and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStopResponse {
    /// `true` when the node had to be force-killed because it did not exit
    /// within the cooperative shutdown grace period. `false` when it exited
    /// gracefully.
    pub force_killed: bool,
}

impl NodeStopResponse {
    /// The node exited gracefully within the grace period, or had no
    /// process.
    pub fn graceful() -> Self {
        Self {
            force_killed: false,
        }
    }

    /// The node ignored the cooperative shutdown and had to be force-killed
    /// (SIGKILL to its process group).
    pub fn forced() -> Self {
        Self { force_killed: true }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_stop_response::Builder>();
            response.set_force_killed(self.force_killed);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_stop_response::Reader>()?;
        Ok(Self {
            force_killed: response.get_force_killed(),
        })
    }
}

impl crate::encoding::Wire for NodeStopRequest {
    type Root = crate::node_capnp::node_stop_request::Owned;
}

impl crate::encoding::Wire for NodeStopResponse {
    type Root = crate::node_capnp::node_stop_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_request_round_trips() {
        let request = NodeStopRequest::new("instance-42");
        let payload = request.encode().expect("encode");
        let decoded = NodeStopRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.instance_id, "instance-42");
        assert_eq!(decoded, request);
    }

    #[test]
    fn stop_request_decode_rejects_malformed_bytes() {
        NodeStopRequest::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }

    /// The force-killed flag survives the wire round-trip distinctly from
    /// a graceful stop.
    #[test]
    fn stop_response_round_trips_both_stops() {
        for response in [NodeStopResponse::graceful(), NodeStopResponse::forced()] {
            let payload = response.encode().expect("encode");
            let decoded = NodeStopResponse::decode(payload.as_ref()).expect("decode");
            assert_eq!(decoded, response);
        }
        assert_ne!(NodeStopResponse::graceful(), NodeStopResponse::forced());
    }

    #[test]
    fn stop_response_decode_rejects_malformed_bytes() {
        NodeStopResponse::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }
}
