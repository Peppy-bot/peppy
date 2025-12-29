use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListRequest;

impl NodeListRequest {
    pub fn new() -> Self {
        Self
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<node_capnp::node_list_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader.get_root::<node_capnp::node_list_request::Reader>()?;
        Ok(Self)
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeListResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_LIST,
            Some(target_master_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeListResponse::decode(&response.payload().to_bytes())
    }
}

impl Default for NodeListRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListResponse {
    pub dot_graph: String,
}

impl NodeListResponse {
    pub fn new(dot_graph: impl Into<String>) -> Self {
        Self {
            dot_graph: dot_graph.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_list_response::Builder>();
            response.set_dot_graph(&self.dot_graph);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_list_response::Reader>()?;
        Ok(Self {
            dot_graph: response.get_dot_graph()?.to_str()?.to_owned(),
        })
    }
}
