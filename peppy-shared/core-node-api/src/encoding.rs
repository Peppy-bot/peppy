//! Cap'n Proto encoding utilities for core-node messages.
//!
//! This module provides utilities for encoding and decoding Cap'n Proto messages
//! used in the core-node services.
mod clock;
mod datastore;
mod endpoints;
mod federation;
mod health;
mod info;
mod node;
mod repo;
mod stack;

pub use clock::{
    ClockConsumerInfo, ClockDomainInfo, ClockListRequest, ClockListResponse, ClockOffsetRequest,
    ClockOffsetResponse, ClockRequest, ClockResponse, ClockTick,
};
pub use datastore::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreKey, DatastoreKeyError, DatastoreListEntry,
    DatastoreListRequest, DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreRequest, DatastoreStoreResponse,
};
pub use federation::{
    FederationVerdict, PairCommitRequest, ParticipantInstancesRemoveRequest,
    ParticipantReleaseRequest, ParticipantReserveRequest, ParticipantReserveResponse,
    ParticipantSetsUpdateRequest, ParticipantSliceBeginRequest, ParticipantSliceBeginResponse,
    RelationshipEvent, RelationshipNotification, RelationshipNotificationAck, RemovedInstances,
    RemovedInstancesError, SlotMembers, SlotSet,
};
pub use health::{HealthRequest, HealthResponse};
pub use info::{ContainerInfo, InfoRequest, InfoResponse};
pub use node::builder::FeedbackStream;
pub use node::{
    add::NodeAddFeedback, add::NodeAddGoal, add::NodeAddGoalResponse, add::NodeAddResult,
    add::NodeSource, builder::NodeBuildFeedback, builder::NodeBuildGoal,
    builder::NodeBuildGoalResponse, builder::NodeBuildResult, info::NodeInfo,
    info::NodeInfoRequest, info::NodeInfoResponse, info::NodeInstanceInfo, init::NodeInitRequest,
    init::NodeInitResponse, remove::NodeRemoveRequest, remove::NodeRemoveResponse,
    run::DuplicateObservationTarget, run::NodeRunFeedback, run::NodeRunGoal,
    run::NodeRunGoalResponse, run::NodeRunResult, run::ObservationTarget, run::ObservationTargets,
    run::PairTarget, run::RemotePeerPairing, stop::NodeStopRequest, stop::NodeStopResponse,
    sync::NodeSyncRequest, sync::NodeSyncResponse, sync::RepoResolvedEntry,
};
pub use repo::{
    GitRepoRef, GitRepoRefError, PEPPY_RELEASE_REF, PEPPY_RELEASE_TAG_PREFIX, PeppyBuild, ReadRef,
    RepoAddRequest, RepoAddResponse, RepoExcludeRequest, RepoExcludeResponse, RepoItemKind,
    RepoListNodeEntry, RepoListRepoEntry, RepoListRepoFailure, RepoListRepoFailureKind,
    RepoListRequest, RepoListResponse, RepoRefreshFeedback, RepoRefreshGoal,
    RepoRefreshGoalResponse, RepoRefreshResult, RepoRemoveRequest, RepoRemoveResponse, RepoSource,
    RepoSourceKind,
};
pub use stack::benchmark::{
    BenchmarkFeedbackStep, ClockConfidence, DEFAULT_SAMPLES, InterfaceKind, InterfaceLatency,
    MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal, StackBenchmarkGoalResponse,
    StackBenchmarkResult,
};
pub use stack::budgets::{DEFAULT_IDLE_TIMEOUT_SECS, StackBudgets};
pub use stack::join::{ArgumentOverride, ArgumentOverrideError, JoinPlacement, StackJoinGoal};
pub use stack::launch::{
    InstanceEndpoints, LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse,
    LaunchJoin, LaunchResult, LauncherOrigin, NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
    PlacementSpec, StackBuildGoal,
};
pub use stack::list::{
    CopyInfo, LaunchIdentity, SetMember, StackListRequest, StackListResponse,
    slots_in_first_added_order,
};
pub use stack::remove::StackRemoveGoal;
pub use stack::reset::{StackResetRequest, StackResetResponse};

use capnp::introspect::Introspect;
use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;
use std::path::{Path, PathBuf};

use crate::{Payload, Result};

/// Ties a codec struct to the Cap'n Proto wire root it encodes.
///
/// Implemented next to each codec, in the same file whose `encode`/`decode`
/// bodies name that root, the one place the pairing is a checkable fact.
/// The method registry ([`crate::registry`]) resolves a payload's reflection
/// handle through this trait, so registry entries name only the codec struct.
pub trait Wire {
    /// The generated `Owned` marker of this codec's wire root struct.
    type Root: Introspect;
}

/// A wire integer of 0 is an absent field; it decodes to `default`.
pub(crate) fn with_default<T: PartialEq + Default + Copy>(value: T, default: T) -> T {
    if value == T::default() {
        default
    } else {
        value
    }
}

/// Converts an empty Cap'n Proto text field to `None`, non-empty to `Some(String)`.
pub(crate) fn optional_text(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

/// The counterpart to [`optional_text`]: decodes a text field that the message
/// is not reconstructible without.
///
/// Cap'n Proto defaults an absent text field to the empty string, so a required
/// field and an omitted one are indistinguishable on the wire. Refusing here is
/// what stops a receiver from acting on a defaulted identity. Sites that can
/// name the fix for the operator (a version skew, say) should return their own
/// `Error::Decoding` with that prose instead. This is the default, not a rule.
pub(crate) fn required_text(value: &str, field: &str) -> Result<String> {
    if value.is_empty() {
        return Err(crate::Error::Decoding(format!("`{field}` is empty")));
    }
    Ok(value.to_owned())
}

/// A text field that must be a [`Name`](config::runtime::Name); the refusal
/// names the field.
pub(crate) fn read_name(value: &str, field: &str) -> Result<config::runtime::Name> {
    config::runtime::Name::new(value).map_err(|e| crate::Error::Decoding(format!("`{field}`: {e}")))
}

/// A text field that must be a [`CoreNodeName`](config::runtime::CoreNodeName);
/// the refusal names the field.
pub(crate) fn read_core_node_name(
    value: &str,
    field: &str,
) -> Result<config::runtime::CoreNodeName> {
    config::runtime::CoreNodeName::new(value)
        .map_err(|e| crate::Error::Decoding(format!("`{field}`: {e}")))
}

/// A required text field that must spell a
/// [`Cardinality`](config::node::Cardinality); the refusal names the field and
/// lists the spellings it accepts.
pub(crate) fn read_cardinality(value: &str, field: &str) -> Result<config::node::Cardinality> {
    required_text(value, field)?.parse().map_err(|spelling| {
        crate::Error::Decoding(format!(
            "{field} `{spelling}` is none of one, zero_or_one, one_or_more, zero_or_more"
        ))
    })
}

/// The copy a pair endpoint belongs to: empty text is outside a copy, and any
/// other text must be a [`Name`](config::runtime::Name).
pub(crate) fn read_optional_copy(
    value: &str,
    field: &str,
) -> Result<Option<config::runtime::Name>> {
    if value.is_empty() {
        return Ok(None);
    }
    config::runtime::Name::new(value)
        .map(Some)
        .map_err(|e| crate::Error::Decoding(format!("{field} is not a name: {e}")))
}

/// Refuses an empty entry in a list whose members each name something: a
/// defaulted string in such a list is a hole, not a value.
pub(crate) fn required_text_list(values: Vec<String>, field: &str) -> Result<Vec<String>> {
    for (idx, value) in values.iter().enumerate() {
        required_text(value, &format!("{field}[{idx}]"))?;
    }
    Ok(values)
}

/// Writes strings into an already-initialized `List(Text)` builder.
///
/// Pairs with [`read_text_list`]. Callers size the list themselves through
/// [`capnp_list_len`], because only they can name the field in the error; the
/// assertion here holds the values to that size, which is what makes the pair
/// lossless.
pub(crate) fn write_text_list<S, I>(mut list: capnp::text_list::Builder<'_>, values: I)
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
    I::IntoIter: ExactSizeIterator,
{
    let values = values.into_iter();
    assert_eq!(
        list.len() as usize,
        values.len(),
        "a text list is sized for exactly the values written into it"
    );
    for (index, value) in values.enumerate() {
        list.set(index as u32, value.as_ref());
    }
}

/// Inverse of [`write_text_list`].
pub(crate) fn read_text_list(list: capnp::text_list::Reader<'_>) -> Result<Vec<String>> {
    let mut values = Vec::with_capacity(list.len() as usize);
    for idx in 0..list.len() {
        values.push(list.get(idx)?.to_str()?.to_owned());
    }
    Ok(values)
}

/// A text list whose every entry must be a name; the refusal says which
/// field held the one that is not.
pub(crate) fn read_name_list(
    list: capnp::text_list::Reader<'_>,
    field: &str,
) -> Result<Vec<config::runtime::Name>> {
    read_text_list(list)?
        .iter()
        .map(|name| read_name(name, field))
        .collect()
}

/// The message of a decode refusal, for tests that assert on it.
#[cfg(test)]
pub(crate) fn decoding_error<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result.expect_err("decode should fail") {
        crate::Error::Decoding(message) => message,
        other => panic!("expected a decoding error, got {other:?}"),
    }
}

/// A path that is not UTF-8, for tests of the encoders that refuse one.
#[cfg(all(test, unix))]
pub(crate) fn non_utf8_path() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"))
}

/// The message of an encode refusal, for tests that assert on it.
#[cfg(all(test, unix))]
pub(crate) fn encoding_error<T: std::fmt::Debug>(result: Result<T>) -> String {
    match result.expect_err("encode should fail") {
        crate::Error::Encoding(message) => message,
        other => panic!("expected an encoding error, got {other:?}"),
    }
}

/// The text of a filesystem-path field, for a path the daemon opens as sent.
/// The wire carries paths as text, so a path that is not UTF-8 is refused: a
/// lossy conversion would replace its invalid bytes and name another file.
pub(crate) fn encode_fs_path<'a>(path: &'a Path, field: &str) -> Result<&'a str> {
    path.to_str().ok_or_else(|| {
        crate::Error::Encoding(format!("{field} is not valid UTF-8: {}", path.display()))
    })
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
/// resolution: relative paths would silently anchor at the daemon's
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
///
/// `write_message_to_words` sizes its buffer once from the segment table, so
/// the bytes land in one allocation whose length equals its capacity, and
/// `Bytes::from(Vec<u8>)` takes that buffer over without allocating a shared
/// header for it. Serializing into a `Vec` cannot fail; the function returns
/// a `Result` so that every codec calls it the same way as the fallible encode
/// steps around it.
pub(crate) fn encode_message(message: &Builder<HeapAllocator>) -> Result<Payload> {
    Ok(Payload::from(serialize::write_message_to_words(message)))
}

/// Encode a Cap'n Proto message builder into a [`NonEmptyPayload`].
///
/// Cap'n Proto's framed wire format always emits at least the segment-table
/// header, so the produced payload is non-empty by construction. The
/// `NonEmptyPayload::try_new` here is therefore infallible in practice and
/// the `expect` documents that invariant; if it ever fires it indicates a
/// `capnp::serialize::write_message_to_words` regression rather than a caller
/// bug.
pub(crate) fn encode_message_non_empty(
    message: &Builder<HeapAllocator>,
) -> Result<crate::NonEmptyPayload> {
    let payload = encode_message(message)?;
    Ok(crate::NonEmptyPayload::try_new(payload)
        .expect("capnp serialize::write_message_to_words always emits a non-empty framed buffer"))
}

/// Decode bytes into a Cap'n Proto message reader over `data` itself: the
/// segments are read in place, not copied out first.
pub(crate) fn decode_message(
    mut data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::BufferSegments<&[u8]>>> {
    Ok(serialize::read_message_from_flat_slice(
        &mut data,
        ReaderOptions::default(),
    )?)
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
    fn required_text_rejects_empty_and_names_the_field() {
        let err = required_text("", "MyField").expect_err("empty must fail");
        assert!(err.to_string().contains("MyField"), "got: {err}");
        assert_eq!(
            required_text("value", "MyField").expect("non-empty"),
            "value"
        );
    }

    #[test]
    fn read_cardinality_reads_every_spelling_and_refuses_an_empty_one() {
        for cardinality in [
            config::node::Cardinality::One,
            config::node::Cardinality::ZeroOrOne,
            config::node::Cardinality::OneOrMore,
            config::node::Cardinality::ZeroOrMore,
        ] {
            assert_eq!(
                read_cardinality(cardinality.as_str(), "MyField").expect("a known spelling"),
                cardinality
            );
        }
        assert_eq!(
            decoding_error(read_cardinality("", "MyField")),
            "`MyField` is empty"
        );
    }

    // The write/read pair is only useful if it is exactly lossless, empty list
    // included: an off-by-one in either half would survive a one-element test.
    #[test]
    fn text_list_round_trips() {
        for values in [
            Vec::new(),
            vec!["only".to_owned()],
            vec!["a".to_owned(), String::new(), "c".to_owned()],
        ] {
            let mut message = Builder::new_default();
            {
                let request = message
                    .init_root::<crate::federation_capnp::participant_reserve_request::Builder>();
                let len = capnp_list_len(values.len(), "test").expect("len fits");
                write_text_list(request.init_deployment_pins_json5(len), &values);
            }
            let request = message
                .get_root_as_reader::<crate::federation_capnp::participant_reserve_request::Reader>(
                )
                .expect("root");
            let read = read_text_list(request.get_deployment_pins_json5().expect("list present"))
                .expect("read");
            assert_eq!(read, values);
        }
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

    /// The payload is the standard framed stream that `write_message` writes,
    /// for a one-segment message and for one that spills into more segments.
    #[test]
    fn encode_message_writes_the_framed_stream_bytes() {
        for pin_count in [1_u32, 4096] {
            let mut message = Builder::new_default();
            {
                let request = message
                    .init_root::<crate::federation_capnp::participant_reserve_request::Builder>();
                let mut pins = request.init_deployment_pins_json5(pin_count);
                for index in 0..pin_count {
                    pins.set(index, "{ node: 'pinned' }");
                }
            }
            let mut framed = Vec::new();
            serialize::write_message(&mut framed, &message).expect("write to a Vec");
            assert!(
                pin_count == 1 || message.get_segments_for_output().len() > 1,
                "the large message spans several segments"
            );
            let payload = encode_message(&message).expect("encode");
            assert_eq!(payload.as_ref(), framed.as_slice());
        }
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
