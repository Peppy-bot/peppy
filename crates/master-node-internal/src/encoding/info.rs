//! Cap'n Proto encoding utilities for info messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::info_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoType {
    Uptime,
    HostName,
    MasterNodeName,
    MasterNodeInstanceId,
}

impl From<InfoType> for info_capnp::InfoType {
    fn from(value: InfoType) -> Self {
        match value {
            InfoType::Uptime => info_capnp::InfoType::Uptime,
            InfoType::MasterNodeName => info_capnp::InfoType::MasterNodeName,
            InfoType::MasterNodeInstanceId => info_capnp::InfoType::MasterNodeInstanceId,
            InfoType::HostName => todo!(),
        }
    }
}

impl From<info_capnp::InfoType> for InfoType {
    fn from(value: info_capnp::InfoType) -> Self {
        match value {
            info_capnp::InfoType::Uptime => InfoType::Uptime,
            info_capnp::InfoType::MasterNodeName => InfoType::MasterNodeName,
            info_capnp::InfoType::MasterNodeInstanceId => InfoType::MasterNodeInstanceId,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRequest {
    pub info_type: InfoType,
}

impl InfoRequest {
    pub fn new(info_type: InfoType) -> Self {
        Self { info_type }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<info_capnp::info_request::Builder>();
            request.set_info_type(self.info_type.into());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<info_capnp::info_request::Reader>()?;
        Ok(Self {
            info_type: request.get_info_type()?.into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    pub info_type: InfoType,
    pub value: String,
}

impl InfoResponse {
    pub fn new(info_type: InfoType, value: impl Into<String>) -> Self {
        Self {
            info_type,
            value: value.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<info_capnp::info_response::Builder>();
            response.set_info_type(self.info_type.into());
            response.set_value(&self.value);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<info_capnp::info_response::Reader>()?;
        Ok(Self {
            info_type: response.get_info_type()?.into(),
            value: response.get_value()?.to_str()?.to_owned(),
        })
    }
}
