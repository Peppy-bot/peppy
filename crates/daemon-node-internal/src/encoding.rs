//! Cap'n Proto encoding utilities for daemon-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the daemon-node services.
mod info;
mod launch;
mod node;
mod ping;
mod reset;

pub use info::{InfoRequest, InfoResponse};
pub use launch::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
};
pub use node::{
    add::NodeAddFeedback, add::NodeAddGoal, add::NodeAddGoalResponse, add::NodeAddResult,
    add::NodeSource, info::InterfaceIntegrity, info::NodeInfoRequest, info::NodeInfoResponse,
    init::NodeInitRequest, init::NodeInitResponse, list::NodeListRequest, list::NodeListResponse,
    remove::NodeRemoveRequest, remove::NodeRemoveResponse, start::NodeStartFeedback,
    start::NodeStartGoal, start::NodeStartGoalResponse, start::NodeStartResult,
    stop::NodeStopRequest, stop::NodeStopResponse, sync::NodeSyncRequest, sync::NodeSyncResponse,
};
pub use ping::{PingRequest, PingResponse};
pub use reset::{NodeResetRequest, NodeResetResponse};

use bytes::Bytes;
use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;

use crate::Result;

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
