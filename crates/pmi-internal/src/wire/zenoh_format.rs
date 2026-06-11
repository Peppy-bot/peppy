//! Zenoh-shaped wire format. All keyexpr strings emitted on the bus are built
//! here, and incoming keyexprs are parsed here. No other module in the crate
//! constructs keyexprs directly, which keeps the protocol pinned to one place.
//!
//! The in-process mock adapter mirrors this exact wire shape so the same
//! encoder/parser serves both transports. If a future transport needs a
//! different wire form (MQTT, DDS, etc.), add a sibling module rather than
//! diverging this one.

use crate::wire::{
    ActionWireReceiver, ActionWireSender, DEFAULT_LINK_ID, SenderTarget, ServiceKind,
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
    /// `*/{caller_core}/*/{caller_inst}/topic/{discriminator}/{name}/{tag}/{link_id}/{topic}`:
    /// caller_core is segment index 1, caller_inst is segment index 3,
    /// link_id is segment index 8. The link_id segment is surfaced so
    /// consumer-side filters can drop messages whose producer link_id is
    /// already claimed by a sibling pinned subscription.
    pub(crate) fn parse_topic_keyexpr(
        keyexpr: &str,
    ) -> Result<ParsedTopicKey, ZenohWireParseError> {
        let segments: Vec<&str> = keyexpr.split('/').collect();
        let core_node = extract_caller_segment(segments.get(1).copied(), "caller_core_node")?;
        let instance_id = extract_caller_segment(segments.get(3).copied(), "caller_instance_id")?;
        // link_id sits at index 8 in the topic publish shape and is the
        // signal the consumer-side sibling-precedence filter consults when
        // dropping wildcard topic messages whose producer link_id is
        // already claimed by a pinned subscription. Service reply keyexprs
        // also carry a literal at this index (it's the link_id the
        // responder claimed via `ParsedInboundQuery::claim`), so segment 8
        // is populated there too; an empty value would only appear for a
        // truncated wire shape and is treated as "no link_id" since it can
        // never match a pinned literal.
        let link_id = segments
            .get(8)
            .copied()
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
            .to_string();
        Ok(ParsedTopicKey {
            core_node,
            instance_id,
            link_id,
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
    // Zenoh queryables. The producer declares exactly one queryable per
    // `listen_service` call with `*` at the link_id slot. Producers always
    // advertise under the reserved default `_` link_id; the adapter
    // [`ParsedInboundQuery::claim`] accepts either `*` (from `from_any`
    // consumers) or `_` (post-binding-map consumers) at the link_id slot
    // and drops anything else defensively.

    /// Producer-side queryable keyexpr, declared once per `listen_service`.
    /// Layout `{bound_core}/*/{as_inst}/*/{service_root}` — the `*` slots
    /// match any caller's `core_node` / `instance_id`, and the link_id slot
    /// inside `service_root` is also `*` so a single queryable absorbs the
    /// `*` and `_` literals consumers may send.
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
    /// The link_id slot inside `service_root` is always `*`: producers
    /// advertise under the reserved `_` segment and Zenoh's matcher unifies
    /// the two. The `to_core` / `to_inst` slots use `*` when the caller
    /// broadcasts, replacing the legacy `_any_` marker.
    pub(crate) fn service_get_selector(s: &ServiceWireSender) -> String {
        let root = service_root(
            &s.to_target,
            SINGLE_CHUNK_WILDCARD,
            &s.to_service_name,
            s.kind,
        );
        let target_core = s
            .target_core_node
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        let target_inst = s
            .target_instance_id
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
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
    /// queryable declares `*` at the link_id position, so the selector
    /// carries either `*` (from `from_any` consumers) or `_` (the default
    /// literal) — [`ParsedInboundQuery::claim`] confirms it's one of the two
    /// and resolves to the default `_` segment.
    pub(crate) fn parse_inbound_query(
        receiver: &ServiceWireReceiver,
        query_keyexpr: &str,
        attachment_bytes: &[u8],
    ) -> Result<ParsedInboundQuery, ZenohWireParseError> {
        let mut parts = query_keyexpr.split('/').filter(|s| !s.is_empty());

        // Segment 0 is the consumer's `to_core` slot (may be a literal or `*`);
        // ignored here because the queryable's listen keyexpr already filtered
        // on it via Zenoh's matcher.
        let _to_core = parts
            .next()
            .ok_or(ZenohWireParseError::MissingSegment("target_core_node"))?;
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

        let attachment = ServiceQueryAttachment::decode(attachment_bytes)?;

        Ok(ParsedInboundQuery {
            caller_core,
            caller_inst,
            link_id,
            kind: attachment.kind,
        })
    }

    /// Caller-side attachment bytes for a service query. Carries the request
    /// kind (UserRequest vs Probe) so probes can be discriminated from real
    /// requests without smuggling sentinels through the payload.
    pub(crate) fn service_get_selector_attachment(
        _s: &ServiceWireSender,
        kind: ServiceQueryKind,
    ) -> bytes::Bytes {
        ServiceQueryAttachment { kind }.encode()
    }

    // ─── Actions ──────────────────────────────────────────────────────────

    /// Server-side per-goal feedback publish:
    /// `*/{bound_core}/*/{as_inst}/action/{discriminator}/{name}/{tag}/{link_id}/{as_action}/feedback/{as_inst}/{goal_id}`.
    ///
    /// `link_id` is the link_id parsed from the goal's request keyexpr — the
    /// adapter's `claim()` resolves it to the producer's default `_` segment
    /// before publishing, so feedback rides on the same wire slot the goal
    /// targeted.
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
    /// when the target is not pinned. The link_id slot is always `*` to match
    /// the producer's `_` advertisement via Zenoh's matcher.
    pub(crate) fn action_feedback_subscribe(s: &ActionWireSender, goal_id: &str) -> String {
        let action_root = action_root(&s.to_target, SINGLE_CHUNK_WILDCARD, &s.to_action_name);
        let target_core = s
            .target_core_node
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        let target_inst_segment = s
            .target_instance_id
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        format!(
            "{}/{target_core}/{}/{target_inst_segment}/{action_root}/feedback/{target_inst_segment}/{goal_id}",
            s.as_core_node, s.as_instance_id,
        )
    }

    /// Producer-side liveliness-token keyexpr, declared once per exposed
    /// action:
    /// `action_liveliness/{bound_core}/{as_inst}/{discriminator}/{name}/{tag}/{action}`.
    ///
    /// Liveliness tokens live in Zenoh's liveliness space (a distinct
    /// interest type — regular subscribers never observe them), but the
    /// keyexpr still gets its own `action_liveliness` root segment so it
    /// can never be confused with topic/service shapes, and so the mock
    /// adapter can route on it with the same string matcher.
    pub(crate) fn action_liveliness_token(r: &ActionWireReceiver) -> String {
        let (discriminator, name, tag) = target_segments(Some(&r.as_identity));
        format!(
            "action_liveliness/{}/{}/{discriminator}/{name}/{tag}/{}",
            r.bound_core_node, r.as_instance_id, r.as_action_name,
        )
    }

    /// Consumer-side liveliness watch/probe keyexpr. Mirrors
    /// [`Self::action_liveliness_token`] with the producer-identity slots
    /// taken from the (typically pinned) sender; unpinned slots wildcard.
    pub(crate) fn action_liveliness_watch(s: &ActionWireSender) -> String {
        let (discriminator, name, tag) = target_segments(Some(&s.to_target));
        let target_core = s
            .target_core_node
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        let target_inst = s
            .target_instance_id
            .as_deref()
            .unwrap_or(SINGLE_CHUNK_WILDCARD);
        format!(
            "action_liveliness/{target_core}/{target_inst}/{discriminator}/{name}/{tag}/{}",
            s.to_action_name,
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
/// `link_id` is the producer's bound link_id (segment 8 of the publish
/// shape), surfaced so the consumer-side sibling-precedence filter can
/// drop wildcard topic messages whose link_id is claimed by a sibling
/// pinned subscription on the same `(name, tag)`. Service reply keyexprs
/// also populate this slot (with the responder's claimed link_id inside
/// `service_root`); only truncated or malformed wire shapes leave it
/// empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedTopicKey {
    pub(crate) core_node: String,
    pub(crate) instance_id: String,
    pub(crate) link_id: String,
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
        bytes::Bytes::from_static(if self.is_primary {
            &[0x01u8]
        } else {
            &[0x00u8]
        })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Self {
        let is_primary = bytes.first().is_none_or(|b| *b != 0x00);
        Self { is_primary }
    }
}

/// Whether a service query carries a user request (handler should run) or
/// a discovery probe (auto-replied by the producer's request loop before
/// the handler is invoked). The producer reads this from the query
/// attachment to discriminate the two without inspecting payload bytes,
/// closing the silent-data-loss class that occurred when a user payload
/// happened to start with the legacy `\0peppy_service_probe\0` sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceQueryKind {
    UserRequest,
    Probe,
}

impl ServiceQueryKind {
    fn as_byte(self) -> u8 {
        match self {
            Self::UserRequest => 0x00,
            Self::Probe => 0x01,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::UserRequest),
            0x01 => Some(Self::Probe),
            _ => None,
        }
    }
}

/// Service / action query attachment carrying the request kind so the
/// producer can discriminate user requests from discovery probes without
/// smuggling sentinels through the payload.
///
/// Wire layout (mandatory on every service query):
/// - byte 0: magic + version, `0x03`. Earlier versions also carried a
///   sibling-pinned exclusion set; any peer producing an older magic is
///   mid-rollout and must redeploy.
/// - byte 1: kind discriminator (see [`ServiceQueryKind::as_byte`]).
///
/// Decode is strict: a missing attachment, wrong magic, or unknown kind
/// byte is reported as a [`ZenohWireParseError`] so mid-rollout schema
/// skew surfaces loudly instead of as silent misclassification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceQueryAttachment {
    pub(crate) kind: ServiceQueryKind,
}

impl ServiceQueryAttachment {
    pub(crate) const MAGIC_V3: u8 = 0x03;

    pub(crate) fn encode(&self) -> bytes::Bytes {
        bytes::Bytes::from(vec![Self::MAGIC_V3, self.kind.as_byte()])
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ZenohWireParseError> {
        if bytes.is_empty() {
            return Err(ZenohWireParseError::MissingServiceQueryAttachment);
        }
        if bytes[0] != Self::MAGIC_V3 {
            return Err(ZenohWireParseError::ServiceQueryAttachmentMagicMismatch {
                expected: Self::MAGIC_V3,
                got: bytes[0],
            });
        }
        let kind_byte = *bytes
            .get(1)
            .ok_or(ZenohWireParseError::TruncatedServiceQueryAttachment)?;
        let kind = ServiceQueryKind::from_byte(kind_byte)
            .ok_or(ZenohWireParseError::UnknownServiceQueryKind(kind_byte))?;
        Ok(Self { kind })
    }
}

/// Service reply kind, encoded in the [`ServiceReplyAttachment`] that
/// rides on every `query.reply()`. The consumer's poll loop matches on
/// this to skip ACKs, return regular responses, and surface handler
/// errors — without inspecting payload bytes for legacy sentinels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceReplyKind {
    /// Sent immediately by the producer when a real user request arrives,
    /// before the user handler is invoked. Lets the consumer distinguish
    /// `ServiceUnreachable` (no ACK at all) from `ServiceTimeout`
    /// (ACK arrived, handler didn't reply in time). Payload is empty.
    Ack,
    /// The user handler returned `Ok(payload)` — payload bytes are the
    /// handler's response, opaque to the framework. Also used for the
    /// producer's transparent reply to a `Probe` request (empty payload).
    Response,
    /// The user handler returned `Err(reason)` (or a Python handler raised
    /// an exception). Payload bytes are the UTF-8 reason; the consumer
    /// surfaces this as [`crate::error::Error::ServiceError`] (in peppylib).
    HandlerError,
}

impl ServiceReplyKind {
    fn as_byte(self) -> u8 {
        match self {
            Self::Ack => 0x00,
            Self::Response => 0x01,
            Self::HandlerError => 0x02,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Ack),
            0x01 => Some(Self::Response),
            0x02 => Some(Self::HandlerError),
            _ => None,
        }
    }
}

/// Reply attachment carrying the [`ServiceReplyKind`]. Mandatory on every
/// service reply.
///
/// Wire layout: `[0x01 magic][kind u8]` — 2 bytes total. Reasons for
/// [`ServiceReplyKind::HandlerError`] ride in the reply payload as UTF-8
/// (variable length stays out of the attachment lane), keeping this
/// attachment compact and fixed-size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServiceReplyAttachment {
    pub(crate) kind: ServiceReplyKind,
}

impl ServiceReplyAttachment {
    pub(crate) const MAGIC_V1: u8 = 0x01;

    pub(crate) fn encode(self) -> bytes::Bytes {
        bytes::Bytes::from(vec![Self::MAGIC_V1, self.kind.as_byte()])
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ZenohWireParseError> {
        if bytes.is_empty() {
            return Err(ZenohWireParseError::MissingServiceReplyAttachment);
        }
        if bytes[0] != Self::MAGIC_V1 {
            return Err(ZenohWireParseError::ServiceReplyAttachmentMagicMismatch {
                expected: Self::MAGIC_V1,
                got: bytes[0],
            });
        }
        let kind_byte = *bytes
            .get(1)
            .ok_or(ZenohWireParseError::TruncatedServiceReplyAttachment)?;
        let kind = ServiceReplyKind::from_byte(kind_byte)
            .ok_or(ZenohWireParseError::UnknownServiceReplyKind(kind_byte))?;
        Ok(Self { kind })
    }
}

/// Result of parsing an inbound queryable selector. Carries the
/// caller-identity slots plus the link_id slot from the selector — the
/// producer's single queryable declares `*` at the link_id slot, so the
/// adapter inspects this field via [`Self::claim`] to confirm the selector
/// targeted the producer's default `_` segment (or a `*` wildcard).
///
/// `kind` is decoded from the query attachment and distinguishes user
/// requests (handler runs) from probes (auto-replied without invoking the
/// handler).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedInboundQuery {
    pub(crate) caller_core: String,
    pub(crate) caller_inst: String,
    /// Raw value of the link_id slot in the selector. Either the
    /// single-chunk wildcard `*` (a `from_any` consumer) or the default
    /// `_` literal (a pinned consumer, post-binding-map era).
    pub(crate) link_id: String,
    /// Whether this query is a real user request or a discovery probe.
    /// Decoded from the mandatory query attachment.
    pub(crate) kind: ServiceQueryKind,
}

impl ParsedInboundQuery {
    /// Confirms the inbound link_id slot is one the producer answers to and
    /// returns the literal the producer should claim (always
    /// [`DEFAULT_LINK_ID`]). Wildcard `*` and the literal `_` both succeed;
    /// anything else returns `None` so the adapter drops the query without
    /// replying. Unreachable in practice because Zenoh's keyexpr matcher
    /// already filtered the selector against the producer's queryable, but
    /// kept as a defensive guard against mid-rollout schema skew.
    pub(crate) fn claim(&self) -> Option<&'static str> {
        match self.link_id.as_str() {
            SINGLE_CHUNK_WILDCARD | DEFAULT_LINK_ID => Some(DEFAULT_LINK_ID),
            _ => None,
        }
    }
}

/// Reasons a request keyexpr or attachment can fail to match the expected
/// wire shape. Mid-rollout schema skew surfaces as a structured error
/// (one of these) rather than silently falling back to a default — the
/// load-bearing safety property for the sentinel removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZenohWireParseError {
    MissingSegment(&'static str),
    WildcardInCallerSegment(&'static str),
    ServiceRootMismatch {
        expected: String,
        got: String,
    },
    /// Inbound service query arrived with no attachment. Either an
    /// old-protocol peer or a non-peppy client. The producer must drop
    /// the query (no reply) so the consumer sees `ServiceUnreachable`.
    MissingServiceQueryAttachment,
    ServiceQueryAttachmentMagicMismatch {
        expected: u8,
        got: u8,
    },
    TruncatedServiceQueryAttachment,
    UnknownServiceQueryKind(u8),
    /// Service reply arrived with no attachment. Treated as a malformed
    /// reply and dropped — the consumer's poll loop ignores it and the
    /// usual timeout / unreachable path applies.
    MissingServiceReplyAttachment,
    ServiceReplyAttachmentMagicMismatch {
        expected: u8,
        got: u8,
    },
    TruncatedServiceReplyAttachment,
    UnknownServiceReplyKind(u8),
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
            Self::MissingServiceQueryAttachment => write!(
                f,
                "service query arrived with no attachment (peer is on an older protocol)"
            ),
            Self::ServiceQueryAttachmentMagicMismatch { expected, got } => write!(
                f,
                "service query attachment magic mismatch: expected {expected:#04x}, got {got:#04x}"
            ),
            Self::TruncatedServiceQueryAttachment => {
                write!(f, "service query attachment truncated")
            }
            Self::UnknownServiceQueryKind(byte) => {
                write!(f, "unknown service query kind discriminator: {byte:#04x}")
            }
            Self::MissingServiceReplyAttachment => {
                write!(f, "service reply arrived with no attachment")
            }
            Self::ServiceReplyAttachmentMagicMismatch { expected, got } => write!(
                f,
                "service reply attachment magic mismatch: expected {expected:#04x}, got {got:#04x}"
            ),
            Self::TruncatedServiceReplyAttachment => {
                write!(f, "service reply attachment truncated")
            }
            Self::UnknownServiceReplyKind(byte) => {
                write!(f, "unknown service reply kind discriminator: {byte:#04x}")
            }
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
