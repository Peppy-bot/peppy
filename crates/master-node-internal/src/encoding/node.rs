//! Cap'n Proto encoding utilities for node messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeCmd {
    Add,
    List,
    Synchronize,
}

impl From<NodeCmd> for node_capnp::NodeCmd {
    fn from(value: NodeCmd) -> Self {
        match value {
            NodeCmd::Add => node_capnp::NodeCmd::Add,
            NodeCmd::List => node_capnp::NodeCmd::List,
            NodeCmd::Synchronize => node_capnp::NodeCmd::Synchronize,
        }
    }
}

impl From<node_capnp::NodeCmd> for NodeCmd {
    fn from(value: node_capnp::NodeCmd) -> Self {
        match value {
            node_capnp::NodeCmd::Add => NodeCmd::Add,
            node_capnp::NodeCmd::List => NodeCmd::List,
            node_capnp::NodeCmd::Synchronize => NodeCmd::Synchronize,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRequest {
    pub cmd: NodeCmd,
}

impl NodeRequest {
    pub fn new(cmd: NodeCmd) -> Self {
        Self { cmd }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_request::Builder>();
            request.set_cmd(self.cmd.into());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_request::Reader>()?;
        Ok(Self {
            cmd: request.get_cmd()?.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeResponse {
    pub cmd: NodeCmd,
    pub value: String,
}

impl NodeResponse {
    pub fn new(cmd: NodeCmd, value: impl Into<String>) -> Self {
        Self {
            cmd,
            value: value.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_response::Builder>();
            response.set_cmd(self.cmd.into());
            response.set_value(&self.value);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_response::Reader>()?;
        Ok(Self {
            cmd: response.get_cmd()?.into(),
            value: response.get_value()?.to_str()?.to_owned(),
        })
    }
}
