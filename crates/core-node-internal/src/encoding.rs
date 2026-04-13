//! Cap'n Proto encoding utilities for core-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the core-node services.
mod info;
mod node;
mod ping;
mod repo;
mod stack;

// Note: there used to be a top-level `builder` module here. Build encoding
// now lives at `node::builder` alongside `node::add`.

pub use info::{ContainerInfo, InfoRequest, InfoResponse};
pub use node::{
    add::NodeAddFeedback, add::NodeAddGoal, add::NodeAddGoalResponse, add::NodeAddResult,
    add::NodeSource, builder::NodeBuildFeedback, builder::NodeBuildGoal,
    builder::NodeBuildGoalResponse, builder::NodeBuildResult, info::NodeInfo,
    info::NodeInfoRequest, info::NodeInfoResponse, info::NodeInstanceInfo, init::NodeInitRequest,
    init::NodeInitResponse, remove::NodeRemoveRequest, remove::NodeRemoveResponse,
    run::NodeRunFeedback, run::NodeRunGoal, run::NodeRunGoalResponse, run::NodeRunResult,
    stop::NodeStopRequest, stop::NodeStopResponse, sync::NodeSyncRequest, sync::NodeSyncResponse,
};
pub use ping::{PingRequest, PingResponse};
pub use repo::{
    RepoAddRequest, RepoAddResponse, RepoExcludeRequest, RepoExcludeResponse, RepoListNodeEntry,
    RepoListRequest, RepoListResponse, RepoRefreshFeedback, RepoRefreshGoal,
    RepoRefreshGoalResponse, RepoRefreshResult, RepoRemoveRequest, RepoRemoveResponse, RepoSource,
    RepoSourceKind,
};
pub use stack::launch::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
};
pub use stack::list::{StackListRequest, StackListResponse};
pub use stack::reset::{NodeResetRequest, NodeResetResponse};

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
