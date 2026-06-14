//! Cap'n Proto encoding utilities for core-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the core-node services.
mod clock;
mod datastore;
mod info;
mod node;
mod ping;
mod repo;
mod stack;

// Note: there used to be a top-level `builder` module here. Build encoding
// now lives at `node::builder` alongside `node::add`.

pub use clock::{ClockOffsetRequest, ClockOffsetResponse, ClockRequest, ClockResponse, ClockTick};
pub use datastore::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreKey, DatastoreKeyError, DatastoreListEntry,
    DatastoreListRequest, DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreRequest, DatastoreStoreResponse,
};
pub use info::{ContainerInfo, InfoRequest, InfoResponse};
pub use node::builder::FeedbackStream;
pub use node::{
    add::NodeAddFeedback, add::NodeAddGoal, add::NodeAddGoalResponse, add::NodeAddResult,
    add::NodeSource, builder::NodeBuildFeedback, builder::NodeBuildGoal,
    builder::NodeBuildGoalResponse, builder::NodeBuildResult, info::NodeInfo,
    info::NodeInfoRequest, info::NodeInfoResponse, info::NodeInstanceInfo, init::NodeInitRequest,
    init::NodeInitResponse, remove::NodeRemoveRequest, remove::NodeRemoveResponse,
    run::NodeRunFeedback, run::NodeRunGoal, run::NodeRunGoalResponse, run::NodeRunResult,
    stop::NodeStopRequest, stop::NodeStopResponse, sync::NodeSyncRequest, sync::NodeSyncResponse,
    sync::RepoResolvedEntry,
};
pub use ping::{PingRequest, PingResponse};
pub use repo::{
    RepoAddRequest, RepoAddResponse, RepoExcludeRequest, RepoExcludeResponse, RepoItemKind,
    RepoListNodeEntry, RepoListRequest, RepoListResponse, RepoRefreshFeedback, RepoRefreshGoal,
    RepoRefreshGoalResponse, RepoRefreshResult, RepoRemoveRequest, RepoRemoveResponse, RepoSource,
    RepoSourceKind,
};
pub use stack::benchmark::{
    BenchmarkFeedbackStep, ClockConfidence, DEFAULT_SAMPLES, InterfaceKind, InterfaceLatency,
    MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal, StackBenchmarkGoalResponse,
    StackBenchmarkResult,
};
pub use stack::launch::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    LauncherOrigin, NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
};
pub use stack::list::{StackListRequest, StackListResponse};
pub use stack::reset::{NodeResetRequest, NodeResetResponse};

use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;
use std::path::PathBuf;

use crate::{Payload, Result};

/// Converts an empty Cap'n Proto text field to `None`, non-empty to `Some(String)`.
pub(crate) fn optional_text(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// Decode a non-empty filesystem-path text field into a `PathBuf`.
/// Relative paths are accepted; sites that name a real location should use
/// [`decode_absolute_fs_path`] instead.
pub(crate) fn decode_fs_path(path: &str, label: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(crate::Error::Decoding(format!("{label}: path is empty")));
    }
    Ok(PathBuf::from(path))
}

/// Like [`decode_fs_path`] but additionally requires an absolute path.
/// Use this when the daemon will open the path without further
/// resolution — relative paths would silently anchor at the daemon's
/// CWD, which is a footgun.
pub(crate) fn decode_absolute_fs_path(path: &str, label: &str) -> Result<PathBuf> {
    let buf = decode_fs_path(path, label)?;
    if !buf.is_absolute() {
        return Err(crate::Error::Decoding(format!(
            "{label}: path must be absolute, got `{path}`"
        )));
    }
    Ok(buf)
}

pub(crate) fn capnp_list_len(len: usize, field: &str) -> Result<u32> {
    len.try_into().map_err(|_| {
        crate::Error::Encoding(format!(
            "{field} length {len} exceeds Cap'n Proto u32 list limit"
        ))
    })
}

/// Encode a Cap'n Proto message builder into a `Payload`.
pub(crate) fn encode_message(message: &Builder<HeapAllocator>) -> Result<Payload> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)?;
    Ok(Payload::from(buffer))
}

/// Encode a Cap'n Proto message builder into a [`NonEmptyPayload`].
///
/// Cap'n Proto's framed wire format always emits at least the segment-table
/// header, so the produced payload is non-empty by construction. The
/// `NonEmptyPayload::try_new` here is therefore infallible in practice and
/// the `expect` documents that invariant; if it ever fires it indicates a
/// `capnp::serialize::write_message` regression rather than a caller bug.
pub(crate) fn encode_message_non_empty(
    message: &Builder<HeapAllocator>,
) -> Result<crate::NonEmptyPayload> {
    let payload = encode_message(message)?;
    Ok(crate::NonEmptyPayload::try_new(payload)
        .expect("capnp serialize::write_message always emits a non-empty framed buffer"))
}

/// Decode bytes into a Cap'n Proto message reader.
pub(crate) fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    Ok(serialize::read_message(data, ReaderOptions::default())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_fs_path_rejects_empty() {
        let err = decode_fs_path("", "TestLabel").expect_err("empty must fail");
        assert!(err.to_string().contains("TestLabel"));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn decode_fs_path_accepts_relative() {
        let buf = decode_fs_path("rel/path", "TestLabel").expect("relative must pass");
        assert_eq!(buf, PathBuf::from("rel/path"));
    }

    #[test]
    fn decode_absolute_fs_path_rejects_relative() {
        let err = decode_absolute_fs_path("rel/path", "TestLabel").expect_err("relative must fail");
        let msg = err.to_string();
        assert!(msg.contains("TestLabel"), "got: {msg}");
        assert!(msg.contains("absolute"), "got: {msg}");
        assert!(msg.contains("rel/path"), "got: {msg}");
    }

    #[test]
    fn decode_absolute_fs_path_accepts_absolute() {
        let buf = decode_absolute_fs_path("/abs/path", "TestLabel").expect("absolute must pass");
        assert_eq!(buf, PathBuf::from("/abs/path"));
    }

    #[test]
    fn optional_text_maps_empty_to_none() {
        assert_eq!(optional_text(""), None);
        assert_eq!(optional_text("value"), Some("value".to_owned()));
    }

    #[test]
    fn capnp_list_len_accepts_in_range() {
        assert_eq!(capnp_list_len(0, "f").expect("zero fits"), 0);
        let max = u32::MAX as usize;
        assert_eq!(capnp_list_len(max, "f").expect("u32::MAX fits"), u32::MAX);
    }

    // `u32::MAX + 1` only overflows the cast on 64-bit `usize`; skip where a
    // `usize` cannot represent it so the test stays meaningful, not vacuous.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn capnp_list_len_rejects_overflow() {
        let too_big = u32::MAX as usize + 1;
        let err = capnp_list_len(too_big, "MyField").expect_err("over-u32 must fail");
        let msg = err.to_string();
        assert!(msg.contains("MyField"), "got: {msg}");
        assert!(msg.contains("exceeds"), "got: {msg}");
    }

    #[test]
    fn encode_message_non_empty_yields_decodable_non_empty_payload() {
        // Any builder serializes to at least the capnp segment-table header, so
        // the non-empty wrapper construction is infallible and the bytes decode.
        let mut builder = Builder::new_default();
        builder.init_root::<crate::clock_capnp::clock_request::Builder>();
        let payload = encode_message_non_empty(&builder)
            .expect("non-empty wrap")
            .into_inner();
        assert!(!payload.is_empty());
        decode_message(&payload).expect("framed bytes decode");
    }
}
