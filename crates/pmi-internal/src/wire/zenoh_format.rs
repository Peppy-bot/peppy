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

    // ─── Topic attachment ─────────────────────────────────────────────────
    //
    // A producer bound to N link_ids issues N `put`s per `emit` because Zenoh
    // `put` keyexprs can't carry wildcards. A subscriber that wildcards the
    // link_id slot intersects all N — without help, it receives the same
    // payload N times per emit. The producer marks the publish for
    // `effective[0]` (first-bound) as primary and the rest as secondary; the
    // adapter drops secondaries for wildcard subscribers. Pinned subscribers
    // ignore the marker because their keyexpr already filters to a single
    // publish per emit. This is the topic-side analog of the service
    // "first-bound dispatch" pattern in [`ParsedInboundQuery::choose_link_id`].

    // ─── Services ─────────────────────────────────────────────────────────
    //
    // Services and action sub-services (goal / cancel / result) ride on
    // Zenoh queryables. The producer declares exactly one queryable per
    // `listen_service` call with `*` at the link_id slot, regardless of how
    // many link_ids the receiver binds. Producer-side dispatch (in the
    // adapter's `handle_queryable`) picks the concrete link_id from the
    // bound set when claiming each inbound query — `from_any` consumers
    // claim `bound_link_ids[0]`, pinned consumers claim the literal they
    // sent. This keeps the user handler firing exactly once per consumer
    // call, even when a single producer process binds N link_ids.

    /// Producer-side queryable keyexpr, declared once per `listen_service`.
    /// Layout `{bound_core}/*/{as_inst}/*/{service_root}` — the `*` slots
    /// match any caller's `core_node` / `instance_id`, and the link_id slot
    /// inside `service_root` is also `*` so a single queryable absorbs every
    /// link_id literal the producer binds. The adapter resolves which
    /// concrete link_id to bind each request to after parsing the selector.
    pub(crate) fn service_queryable_declare(r: &ServiceWireReceiver) -> String {
        let root = service_root(
            &r.as_identity,
            SINGLE_CHUNK_WILDCARD,
            &r.as_service_name,
            r.kind,
        );
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
    /// caller's identity slots and the link_id slot. The producer's single
    /// queryable declares `*` at the link_id position, so the selector's
    /// literal-or-`*` at that slot is the only signal of which bound link_id
    /// the consumer wanted — [`ParsedInboundQuery::choose_link_id`] resolves
    /// that into a concrete claim from the producer's bound set.
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

        let link_id = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("link_id"))?
            .to_string();

        Ok(ParsedInboundQuery {
            caller_core,
            caller_inst,
            link_id,
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

/// Topic-publish attachment marker. See the comment block in the topic
/// section of [`ZenohWireFormat`] for the rationale. One byte on the wire:
/// `0x01` = primary, `0x00` = secondary. A missing or empty attachment
/// decodes as primary so producers that don't set it (no path today,
/// defensive) behave as if every publish is the only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TopicAttachment {
    pub(crate) is_primary: bool,
}

impl TopicAttachment {
    pub(crate) fn encode(&self) -> bytes::Bytes {
        bytes::Bytes::from_static(if self.is_primary { &[0x01u8] } else { &[0x00u8] })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Self {
        let is_primary = bytes.first().is_none_or(|b| *b != 0x00);
        Self { is_primary }
    }
}

/// Result of parsing an inbound queryable selector. Carries the
/// caller-identity slots plus the link_id slot from the selector — the
/// producer's single queryable declares `*` at the link_id slot, so the
/// adapter inspects this field to decide which of its bound link_ids to
/// claim for the inbound request via [`Self::choose_link_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedInboundQuery {
    pub(crate) caller_core: String,
    pub(crate) caller_inst: String,
    /// Raw value of the link_id slot in the selector. Either the
    /// single-chunk wildcard `*` (a `from_any` consumer) or a concrete
    /// literal (a pinned consumer).
    pub(crate) link_id: String,
}

impl ParsedInboundQuery {
    /// Resolves the link_id the producer should bind this request to.
    ///
    /// - Wildcard selector (`from_any` consumer): claims `bound_link_ids[0]`.
    ///   First-bound is deterministic and keeps `ctx.link_id()` stable across
    ///   an action's goal / cancel / result sub-services, which dispatch
    ///   independently but must agree on the link_id for a given goal_id.
    /// - Literal selector matching a bound link_id: claims that literal.
    /// - Literal selector NOT in the bound set: returns `None`, signaling
    ///   the adapter to drop the query without replying. Unreachable in
    ///   practice because Zenoh's keyexpr matcher already filtered the
    ///   selector against the producer's queryable, but kept as a defensive
    ///   guard against mid-rollout schema skew.
    pub(crate) fn choose_link_id<'a>(&self, bound_link_ids: &'a [String]) -> Option<&'a str> {
        if self.link_id == SINGLE_CHUNK_WILDCARD {
            return bound_link_ids.first().map(String::as_str);
        }
        bound_link_ids
            .iter()
            .find(|b| b.as_str() == self.link_id)
            .map(String::as_str)
    }
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
