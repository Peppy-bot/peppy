//! Zenoh-shaped wire format. All keyexpr strings emitted on the bus are built
//! here, and incoming keyexprs are parsed here. No other module in the crate
//! constructs keyexprs directly, which keeps the protocol pinned to one place.
//!
//! The in-process mock adapter mirrors this exact wire shape so the same
//! encoder/parser serves both transports. If a future transport needs a
//! different wire form (MQTT, DDS, etc.), add a sibling module rather than
//! diverging this one.

use crate::wire::{
    ActionWireReceiver, ActionWireSender, BROADCAST_MARKER, SenderTarget, ServiceKind,
    ServiceWireReceiver, ServiceWireSender, TopicWireReceiver, TopicWireSender,
};
use std::fmt;

/// Single-chunk wildcard. Matches exactly one path segment.
const SINGLE_CHUNK_WILDCARD: &str = "*";

/// Returns the three wire segments `(discriminator, name, tag)` for a target.
/// When the target is `None` (untargeted receiver), all three become the
/// single-chunk wildcard so the keyexpr matches any publisher's emission.
fn target_segments(target: Option<&SenderTarget>) -> (&str, &str, &str) {
    match target {
        Some(t) => (t.discriminator(), t.name(), t.tag()),
        None => (
            SINGLE_CHUNK_WILDCARD,
            SINGLE_CHUNK_WILDCARD,
            SINGLE_CHUNK_WILDCARD,
        ),
    }
}

/// Namespace for the zenoh wire format functions. Calls look like
/// `ZenohWireFormat::topic_publish(&sender)`.
pub(crate) struct ZenohWireFormat;

impl ZenohWireFormat {
    // ─── Topics ───────────────────────────────────────────────────────────

    /// Parses the publisher half of a topic keyexpr into the caller's
    /// addressing. Inverse of [`Self::topic_publish`].
    ///
    /// The publish shape is
    /// `*/{caller_core}/*/{caller_inst}/topic/{discriminator}/{name}/{tag}/{topic}`
    /// — caller_core is segment index 1, caller_inst is segment index 3.
    pub(crate) fn parse_topic_keyexpr(
        keyexpr: &str,
    ) -> Result<ParsedTopicKey, ZenohWireParseError> {
        let mut segments = keyexpr.splitn(5, '/');
        let _target_core = segments.next();
        let core_node = extract_caller_segment(segments.next(), "caller_core_node")?;
        let _target_instance = segments.next();
        let instance_id = extract_caller_segment(segments.next(), "caller_instance_id")?;
        Ok(ParsedTopicKey {
            core_node,
            instance_id,
        })
    }

    /// `*/{as_core}/*/{as_inst}/topic/{discriminator}/{name}/{tag}/{link_id}/{as_topic}`
    pub(crate) fn topic_publish(s: &TopicWireSender) -> String {
        let (discriminator, name, tag) = target_segments(Some(&s.as_target));
        format!(
            "{SINGLE_CHUNK_WILDCARD}/{}/{SINGLE_CHUNK_WILDCARD}/{}/topic/{discriminator}/{name}/{tag}/{}/{}",
            s.as_core_node, s.as_instance_id, s.link_id, s.as_topic_name,
        )
    }

    /// `{as_core}/{from_core|*}/{as_inst}/{from_inst|*}/topic/{discriminator|*}/{name|*}/{tag|*}/{link_id|*}/{to_topic}`
    pub(crate) fn topic_subscribe(r: &TopicWireReceiver) -> String {
        let from_core = r.from_core_node.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        let from_inst = r
            .from_instance_id
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        let (discriminator, name, tag) = target_segments(r.from_target.as_ref());
        let link_id = r.from_link_id.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        format!(
            "{}/{from_core}/{}/{from_inst}/topic/{discriminator}/{name}/{tag}/{link_id}/{}",
            r.as_core_node, r.as_instance_id, r.to_topic,
        )
    }

    // ─── Services ─────────────────────────────────────────────────────────

    /// All 4 broadcast-Cartesian listen patterns:
    /// `{bound|_any_}/*/{inst|_any_}/*/{service_root}/request/**`.
    ///
    /// Order matches the original code's `patterns[0..4]` (bound-specific +
    /// instance-specific first, then progressively broader).
    pub(crate) fn service_listen_patterns(r: &ServiceWireReceiver) -> [String; 4] {
        let root = service_root(
            &r.as_identity,
            r.link_id.as_str(),
            &r.as_service_name,
            r.kind,
        );
        let bound = r.bound_core_node.as_str();
        let inst = r.as_instance_id.as_str();
        [
            format!(
                "{bound}/{SINGLE_CHUNK_WILDCARD}/{inst}/{SINGLE_CHUNK_WILDCARD}/{root}/request/**"
            ),
            format!(
                "{bound}/{SINGLE_CHUNK_WILDCARD}/{BROADCAST_MARKER}/{SINGLE_CHUNK_WILDCARD}/{root}/request/**"
            ),
            format!(
                "{BROADCAST_MARKER}/{SINGLE_CHUNK_WILDCARD}/{inst}/{SINGLE_CHUNK_WILDCARD}/{root}/request/**"
            ),
            format!(
                "{BROADCAST_MARKER}/{SINGLE_CHUNK_WILDCARD}/{BROADCAST_MARKER}/{SINGLE_CHUNK_WILDCARD}/{root}/request/**"
            ),
        ]
    }

    /// Client → server request publish:
    /// `{target_core|_any_}/{bound_core}/{target_inst|_any_}/{caller_inst}/{service_root}/request/{request_id}`.
    ///
    /// Defaults `to_link_id` to the reserved `_` segment (matching the
    /// producer-side default) when the consumer didn't pin one. Zenoh
    /// `put` keyexprs can't contain wildcards, so the consumer must
    /// commit to a concrete link_id at publish time; `from_any: true`
    /// service consumers currently fall back to the same `_` and
    /// therefore only reach producers exposed on the default link_id.
    /// Lifting that restriction needs a producer-side cartesian listen
    /// pattern (tracked as a follow-up).
    pub(crate) fn service_request_publish(s: &ServiceWireSender, request_id: &str) -> String {
        let link_id = s
            .to_link_id
            .as_deref()
            .unwrap_or(crate::wire::DEFAULT_LINK_ID);
        let root = service_root(&s.to_target, link_id, &s.to_service_name, s.kind);
        let target_core = s.to_core_node.as_deref().unwrap_or(BROADCAST_MARKER);
        let target_inst = s.to_instance_id.as_deref().unwrap_or(BROADCAST_MARKER);
        format!(
            "{target_core}/{}/{target_inst}/{}/{root}/request/{request_id}",
            s.bound_core_node, s.as_instance_id,
        )
    }

    /// Client-side response subscribe (wildcards on responder fields, keyed by `request_id`):
    /// `{bound_core}/*/{caller_inst}/*/{service_root}/response/{request_id}`.
    pub(crate) fn service_response_subscribe(s: &ServiceWireSender, request_id: &str) -> String {
        let link_id = s
            .to_link_id
            .as_deref()
            .unwrap_or(crate::wire::DEFAULT_LINK_ID);
        let root = service_root(&s.to_target, link_id, &s.to_service_name, s.kind);
        format!(
            "{}/{SINGLE_CHUNK_WILDCARD}/{}/{SINGLE_CHUNK_WILDCARD}/{root}/response/{request_id}",
            s.bound_core_node, s.as_instance_id,
        )
    }

    /// Parses a received request keyexpr against the receiver's expected service
    /// shape and returns the request_id plus the server-side response publish key.
    ///
    /// Returns an error if the keyexpr doesn't match the expected request shape
    /// (wrong segment count, mismatched service_root, missing `request` marker, etc.).
    pub(crate) fn parse_received_request(
        receiver: &ServiceWireReceiver,
        request_keyexpr: &str,
    ) -> Result<ParsedRequest, ZenohWireParseError> {
        let mut parts = request_keyexpr.split('/').filter(|s| !s.is_empty());

        // to_core / to_inst are consumed but unused: the receiver's listen
        // patterns already filter on these via Zenoh keyexpr matching, so any
        // mismatch would have caused the message to land on a different listener.
        let _to_core = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("to_core_node"))?;
        let caller_core = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("caller_core_node"))?;
        let _to_inst = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("to_instance"))?;
        let caller_inst = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("caller_instance"))?;

        let expected_root = service_root(
            &receiver.as_identity,
            receiver.link_id.as_str(),
            &receiver.as_service_name,
            receiver.kind,
        );
        for expected in expected_root.split('/').filter(|s| !s.is_empty()) {
            let got = parts
                .next()
                .ok_or(ZenohWireParseError::MissingSegment("service_root"))?;
            if got != expected {
                return Err(ZenohWireParseError::ServiceRootMismatch {
                    expected: expected.to_string(),
                    got: got.to_string(),
                });
            }
        }

        let marker = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("request"))?;
        if marker != "request" {
            return Err(ZenohWireParseError::NotARequest);
        }

        let request_id = parts
            .next()
            .filter(|s| !s.is_empty())
            .ok_or(ZenohWireParseError::MissingSegment("request_id"))?
            .to_string();

        if parts.next().is_some() {
            return Err(ZenohWireParseError::UnexpectedTrailing);
        }

        // Server-side response publish:
        // {caller_core}/{responder_core}/{caller_inst}/{responder_inst}/{service_root}/response/{request_id}
        let response_keyexpr = format!(
            "{caller_core}/{}/{caller_inst}/{}/{expected_root}/response/{request_id}",
            receiver.bound_core_node, receiver.as_instance_id,
        );

        Ok(ParsedRequest {
            request_id,
            response_keyexpr,
        })
    }

    // ─── Actions ──────────────────────────────────────────────────────────

    /// Server-side per-goal feedback publish:
    /// `*/{bound_core}/*/{as_inst}/action/{discriminator}/{name}/{tag}/{link_id}/{as_action}/feedback/{as_inst}/{goal_id}`.
    pub(crate) fn action_feedback_publish(r: &ActionWireReceiver, goal_id: &str) -> String {
        let action_root = action_root(&r.as_identity, r.link_id.as_str(), &r.as_action_name);
        format!(
            "{SINGLE_CHUNK_WILDCARD}/{}/{SINGLE_CHUNK_WILDCARD}/{}/{action_root}/feedback/{}/{goal_id}",
            r.bound_core_node, r.as_instance_id, r.as_instance_id,
        )
    }

    /// Client-side per-goal feedback subscribe. Wildcards on server-side fields
    /// when the target is not pinned. `to_link_id: None` → match the
    /// producer's link_id slot via the transport wildcard `*`, since
    /// subscribes (unlike publishes) accept wildcards.
    pub(crate) fn action_feedback_subscribe(s: &ActionWireSender, goal_id: &str) -> String {
        let link_id = s.to_link_id.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        let action_root = action_root(&s.to_target, link_id, &s.to_action_name);
        let target_core = s.to_core_node.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        let target_inst_segment = s.to_instance_id.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        format!(
            "{}/{target_core}/{}/{target_inst_segment}/{action_root}/feedback/{target_inst_segment}/{goal_id}",
            s.as_core_node, s.as_instance_id,
        )
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

/// Pulls a caller-identity segment out of a topic keyexpr, rejecting both
/// missing/empty values and the single-chunk wildcard. The publish wire format
/// never places `*` in caller slots, so observing one means the keyexpr is
/// malformed and must not surface to consumers as a real address.
fn extract_caller_segment(
    segment: Option<&str>,
    field: &'static str,
) -> Result<String, ZenohWireParseError> {
    let value = segment
        .filter(|s| !s.is_empty())
        .ok_or(ZenohWireParseError::MissingSegment(field))?;
    if value == SINGLE_CHUNK_WILDCARD {
        return Err(ZenohWireParseError::WildcardInCallerSegment(field));
    }
    Ok(value.to_string())
}

/// Builds the service_root segment. For action sub-services, appends the
/// `goal` / `cancel` / `result` suffix. The `link_id` segment slots between
/// the producer `(name, tag)` pair and the service / action `name`.
fn service_root(target: &SenderTarget, link_id: &str, name: &str, kind: ServiceKind) -> String {
    let suffix = kind.suffix().map(|s| format!("/{s}")).unwrap_or_default();
    format!(
        "{}/{}/{}/{}/{link_id}/{name}{suffix}",
        kind.root_segment(),
        target.discriminator(),
        target.name(),
        target.tag(),
    )
}

/// Builds the action_root segment
/// (`action/{discriminator}/{name}/{tag}/{link_id}/{action}`).
fn action_root(target: &SenderTarget, link_id: &str, action: &str) -> String {
    format!(
        "action/{}/{}/{}/{link_id}/{action}",
        target.discriminator(),
        target.name(),
        target.tag(),
    )
}

// ─── Parsed envelopes returned to the adapter ────────────────────────────

/// Result of parsing the publisher half of a topic keyexpr — extracts the
/// caller's `core_node` and `instance_id` so the adapter can build a
/// [`crate::types::TopicMessage`] without re-parsing the wire string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedTopicKey {
    pub(crate) core_node: String,
    pub(crate) instance_id: String,
}

/// Result of parsing a received service request keyexpr. Fields are
/// `pub(crate)` so the adapter can use the response keyexpr without exposing
/// raw wire strings to peppylib.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedRequest {
    pub(crate) request_id: String,
    /// Server-side response publish keyexpr.
    pub(crate) response_keyexpr: String,
}

/// Reasons a request keyexpr can fail to match the expected request shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZenohWireParseError {
    MissingSegment(&'static str),
    WildcardInCallerSegment(&'static str),
    UnexpectedTrailing,
    NotARequest,
    ServiceRootMismatch { expected: String, got: String },
}

impl fmt::Display for ZenohWireParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSegment(segment) => write!(f, "missing `{segment}` segment in request"),
            Self::WildcardInCallerSegment(segment) => write!(
                f,
                "caller segment `{segment}` must not be the single-chunk wildcard `*`"
            ),
            Self::UnexpectedTrailing => {
                f.write_str("request contains unexpected trailing segments")
            }
            Self::NotARequest => f.write_str("expected `request` marker segment"),
            Self::ServiceRootMismatch { expected, got } => write!(
                f,
                "service root segment mismatch: expected `{expected}`, got `{got}`"
            ),
        }
    }
}

impl std::error::Error for ZenohWireParseError {}

impl From<ZenohWireParseError> for crate::error::Error {
    fn from(err: ZenohWireParseError) -> Self {
        crate::error::Error::BackendError(err.to_string())
    }
}

#[cfg(test)]
mod tests;
