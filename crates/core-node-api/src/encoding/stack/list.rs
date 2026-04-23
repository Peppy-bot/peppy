use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackListRequest {
    with_dot_graph: bool,
}

impl StackListRequest {
    pub fn new(with_dot_graph: bool) -> Self {
        Self { with_dot_graph }
    }

    pub fn with_dot_graph(&self) -> bool {
        self.with_dot_graph
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_list_request::Builder>();
            request.set_with_dot_graph(self.with_dot_graph);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_list_request::Reader>()?;
        Ok(Self {
            with_dot_graph: request.get_with_dot_graph(),
        })
    }
}

impl Default for StackListRequest {
    fn default() -> Self {
        Self::new(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackListResponse {
    pub dot_graph: Option<String>,
    pub graph_json: String,
}

impl StackListResponse {
    pub fn new(dot_graph: Option<String>, graph_json: impl Into<String>) -> Self {
        Self {
            dot_graph,
            graph_json: graph_json.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_list_response::Builder>();
            if let Some(ref dot_graph) = self.dot_graph {
                response.set_dot_graph(dot_graph);
            }
            response.set_graph_json(&self.graph_json);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_list_response::Reader>()?;
        let dot_graph_str = response.get_dot_graph()?.to_str()?.to_owned();
        let dot_graph = if dot_graph_str.is_empty() {
            None
        } else {
            Some(dot_graph_str)
        };
        Ok(Self {
            dot_graph,
            graph_json: response.get_graph_json()?.to_str()?.to_owned(),
        })
    }
}
