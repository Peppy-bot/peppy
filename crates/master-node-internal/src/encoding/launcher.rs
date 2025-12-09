//! Cap'n Proto encoding utilities for launcher messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::messages_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRequest {
    pub peppy_launcher_json5: String,
}

impl LauncherRequest {
    pub fn new(peppy_launcher_json5: impl Into<String>) -> Self {
        Self {
            peppy_launcher_json5: peppy_launcher_json5.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<messages_capnp::launcher_request::Builder>();
            request.set_peppy_launcher_json5(&self.peppy_launcher_json5);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<messages_capnp::launcher_request::Reader>()?;
        Ok(Self {
            peppy_launcher_json5: request.get_peppy_launcher_json5()?.to_str()?.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherResponse {
    pub success: bool,
    pub error_message: String,
}

impl LauncherResponse {
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
            let mut response = builder.init_root::<messages_capnp::launcher_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<messages_capnp::launcher_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

impl Default for LauncherResponse {
    fn default() -> Self {
        Self::new()
    }
}
