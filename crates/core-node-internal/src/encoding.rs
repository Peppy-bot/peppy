//! Cap'n Proto encoding utilities for core-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the core-node services.
mod info;
mod launch;
mod node;
mod ping;
mod reset;

pub use info::{ContainerInfo, InfoRequest, InfoResponse};
pub use launch::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    NodeAddLogEntry, NodeStartLogEntry,
};
pub use node::{
    add::NodeActionFeedback, add::NodeActionGoalResponse, add::NodeAddGoal, add::NodeAddResult,
    add::NodeSource, info::NodeInfoRequest, info::NodeInfoResponse, info::NodeInstanceInfo,
    init::NodeInitRequest, init::NodeInitResponse, list::NodeListRequest, list::NodeListResponse,
    node_build::NodeBuildGoal, node_build::NodeBuildResult, remove::NodeRemoveRequest,
    remove::NodeRemoveResponse, start::NodeStartGoal, start::NodeStartResult,
    stop::NodeStopRequest, stop::NodeStopResponse, sync::NodeSyncRequest, sync::NodeSyncResponse,
};
pub use ping::{PingRequest, PingResponse};
pub use reset::{NodeResetRequest, NodeResetResponse};

use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;

use crate::Result;

use peppylib::types::Payload;

/// Converts an empty Cap'n Proto text field to `None`, non-empty to `Some(String)`.
pub(crate) fn optional_text(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Encode a Cap'n Proto message builder into bytes.
pub(crate) fn encode_message(message: &Builder<HeapAllocator>) -> Result<Payload> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)?;
    Ok(Payload::from(buffer))
}

/// Decode bytes into a Cap'n Proto message reader.
pub(crate) fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    Ok(serialize::read_message(data, ReaderOptions::default())?)
}
