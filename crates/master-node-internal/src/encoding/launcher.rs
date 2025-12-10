//! Cap'n Proto encoding utilities for launcher messages.

use std::path::PathBuf;

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::launcher_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherRequest {
    pub peppy_launcher_json5: String,
    pub from_directory: PathBuf,
}

impl LauncherRequest {
    pub fn new(
        peppy_launcher_json5: impl Into<String>,
        from_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            peppy_launcher_json5: peppy_launcher_json5.into(),
            from_directory: from_directory.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<launcher_capnp::launcher_request::Builder>();
            request.set_peppy_launcher_json5(&self.peppy_launcher_json5);
            request.set_from_directory(&self.from_directory.to_string_lossy());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<launcher_capnp::launcher_request::Reader>()?;
        Ok(Self {
            peppy_launcher_json5: request.get_peppy_launcher_json5()?.to_str()?.to_owned(),
            from_directory: PathBuf::from(request.get_from_directory()?.to_str()?),
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
            let mut response = builder.init_root::<launcher_capnp::launcher_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<launcher_capnp::launcher_response::Reader>()?;
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
