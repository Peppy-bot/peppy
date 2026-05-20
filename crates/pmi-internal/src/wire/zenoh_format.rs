//! Zenoh-shaped wire format. All keyexpr strings emitted on the bus are built
//! here, and incoming keyexprs are parsed here. No other module in the crate
//! constructs keyexprs directly, which keeps the protocol pinned to one place.
//!
//! The in-process mock adapter mirrors this exact wire shape so the same
//! encoder/parser serves both transports. If a future transport needs a
//! different wire form (MQTT, DDS, etc.), add a sibling module rather than
//! diverging this one.

use crate::wire::{
    ActionWireReceiver, ActionWireSender, SenderTarget, ServiceKind, ServiceWireReceiver,
    ServiceWireSender, TopicWireReceiver, TopicWireSender,
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
    //
    // Services and action sub-services (goal / cancel / result) ride on
    // Zenoh queryables. The producer declares one queryable per bound
    // link_id; the consumer's `session.get(selector)` matches via Zenoh's
    // native keyexpr matcher. No `_any_` broadcast marker, no `request_id`
    // tail, no dispatch-time link_id filter — the single-chunk Zenoh
    // wildcard `*` carries the same intent for broadcast slots and for
    // `from_any: true` consumers, and `session.get` accepts wildcards
    // (unlike `put`).

    /// Producer-side queryable keyexpr, declared once per bound link_id.
    /// Layout `{bound_core}/*/{as_inst}/*/{service_root}` — the `*` slots
    /// match any caller's `core_node` / `instance_id`, and the link_id slot
    /// inside `service_root` is the concrete `link_id_literal`.
    pub(crate) fn service_queryable_declare(
        r: &ServiceWireReceiver,
        link_id_literal: &str,
    ) -> String {
        let root = service_root(&r.as_identity, link_id_literal, &r.as_service_name, r.kind);
        format!(
            "{}/{SINGLE_CHUNK_WILDCARD}/{}/{SINGLE_CHUNK_WILDCARD}/{root}",
            r.bound_core_node, r.as_instance_id,
        )
    }

    /// Caller-side get selector. Layout
    /// `{to_core|*}/{bound_core_caller}/{to_inst|*}/{caller_inst}/{service_root}`.
    ///
    /// The link_id slot inside `service_root` is `*` when the caller didn't
    /// pin one (the `from_any: true` fix — `get` selectors may carry Zenoh
    /// wildcards, unlike `put` keyexprs) and a concrete literal otherwise.
    /// Likewise the `to_core` / `to_inst` slots use `*` when the caller
    /// broadcasts, replacing the legacy `_any_` marker.
    pub(crate) fn service_get_selector(s: &ServiceWireSender) -> String {
        let link_id = s.to_link_id.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        let root = service_root(&s.to_target, link_id, &s.to_service_name, s.kind);
        let target_core = s.to_core_node.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        let target_inst = s.to_instance_id.as_deref().unwrap_or(SINGLE_CHUNK_WILDCARD);
        format!(
            "{target_core}/{}/{target_inst}/{}/{root}",
            s.bound_core_node, s.as_instance_id,
        )
    }

    /// Concrete topic-shape reply keyexpr passed to `query.reply()`. Builds
    /// `{caller_core}/{bound_core_producer}/{caller_inst}/{as_inst_producer}/{service_root_with_link_id_literal}`,
    /// so the caller's [`ZenohWireFormat::parse_topic_keyexpr`] surfaces the
    /// responder's `(core_node, instance_id)` to the user.
    pub(crate) fn service_reply_keyexpr(
        r: &ServiceWireReceiver,
        link_id_literal: &str,
        caller_core: &str,
        caller_inst: &str,
    ) -> String {
        let root = service_root(&r.as_identity, link_id_literal, &r.as_service_name, r.kind);
        format!(
            "{caller_core}/{}/{caller_inst}/{}/{root}",
            r.bound_core_node, r.as_instance_id,
        )
    }

    /// Parses a query selector keyexpr (the caller's get-side keyexpr, as
    /// delivered to the producer via `query.key_expr()`) to extract the
    /// caller's identity slots. No link_id parsing: the producer already
    /// knows its own bound link_id from the queryable being drained, and
    /// the dispatch filter is gone — Zenoh keyexpr matching guarantees
    /// the selector only lands on a queryable whose link_id is acceptable.
    pub(crate) fn parse_inbound_query(
        receiver: &ServiceWireReceiver,
        query_keyexpr: &str,
    ) -> Result<ParsedInboundQuery, ZenohWireParseError> {
        let mut parts = query_keyexpr.split('/').filter(|s| !s.is_empty());

        // Segment 0 is the consumer's `to_core` slot (may be a literal or `*`);
        // ignored here because the queryable's listen keyexpr already filtered
        // on it via Zenoh's matcher.
        let _to_core = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("to_core_node"))?;
        let caller_core = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("caller_core_node"))?
            .to_string();
        let _to_inst = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("to_instance"))?;
        let caller_inst = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("caller_instance"))?
            .to_string();

        // Re-validate the service_root prefix segments so a stray
        // matched-but-mismatched selector (e.g. mid-rollout schema skew)
        // surfaces as a structured error rather than a routing surprise.
        for expected in receiver.service_root_prefix_segments() {
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

        Ok(ParsedInboundQuery {
            caller_core,
            caller_inst,
        })
    }

    // ─── Actions ──────────────────────────────────────────────────────────

    /// Server-side per-goal feedback publish:
    /// `*/{bound_core}/*/{as_inst}/action/{discriminator}/{name}/{tag}/{link_id}/{as_action}/feedback/{as_inst}/{goal_id}`.
    ///
    /// `link_id` is the link_id parsed from the goal's request keyexpr, not
    /// the receiver's bound set, since a producer bound to multiple link_ids
    /// publishes feedback addressed to whichever link_id the goal targeted.
    pub(crate) fn action_feedback_publish(
        r: &ActionWireReceiver,
        link_id: &str,
        goal_id: &str,
    ) -> String {
        let action_root = action_root(&r.as_identity, link_id, &r.as_action_name);
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

/// Result of parsing an inbound queryable selector. Carries just the
/// caller-identity slots — the producer's bound link_id is already known
/// to the adapter spawn that owns the matching queryable, and Zenoh's
/// keyexpr matcher has already filtered out anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedInboundQuery {
    pub(crate) caller_core: String,
    pub(crate) caller_inst: String,
}

/// Reasons a request keyexpr can fail to match the expected request shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZenohWireParseError {
    MissingSegment(&'static str),
    WildcardInCallerSegment(&'static str),
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
