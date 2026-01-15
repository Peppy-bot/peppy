//! Cap'n Proto encoding utilities for master-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the master-node services.
mod info;
mod launch;
mod node;
mod ping;
mod reset;

pub use info::{InfoRequest, InfoResponse};
pub use launch::{LaunchRequest, LaunchResponse};
pub use node::{
    add::NodeAddFeedback, add::NodeAddGoal, add::NodeAddGoalResponse, add::NodeAddResult,
    generate::NodeGenerateRequest, generate::NodeGenerateResponse, init::NodeInitRequest,
    init::NodeInitResponse, list::NodeListRequest, list::NodeListResponse,
    remove::NodeRemoveRequest, remove::NodeRemoveResponse, start::NodeStartFeedback,
    start::NodeStartGoal, start::NodeStartResult, stop::NodeStopRequest, stop::NodeStopResponse,
};
pub use ping::{PingRequest, PingResponse};
pub use reset::{NodeResetRequest, NodeResetResponse};

use bytes::Bytes;
use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;

use crate::Result;
use crate::launch_capnp;

/// Encode a Cap'n Proto message builder into bytes.
pub fn encode_message(message: &Builder<HeapAllocator>) -> Result<Bytes> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)?;
    Ok(Bytes::from(buffer))
}

/// Decode bytes into a Cap'n Proto message reader.
pub fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    Ok(serialize::read_message(data, ReaderOptions::default())?)
}

/// Convenience wrapper for building and encoding a launcher response.
pub fn build_launcher_response(success: bool, error_message: &str) -> Result<Bytes> {
    let mut builder = Builder::new_default();
    {
        let mut response = builder.init_root::<launch_capnp::launch_response::Builder>();
        response.set_success(success);
        response.set_error_message(error_message);
    }
    encode_message(&builder)
}
