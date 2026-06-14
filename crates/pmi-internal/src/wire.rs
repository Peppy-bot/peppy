//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / target / topic|service|action / name)
//! is peppy-specific. The zenoh-shaped wire format that encodes it lives in
//! `wire::zenoh_format`.

use config::runtime::ProducerRef;
use std::fmt;

/// A validated keyexpr segment. The wire format builds keyexprs by joining
/// segments with `/`, so a segment must be non-empty, contain no `/`, and not
/// collide with the reserved sentinels (`*`, `**`, `_`) used by the wire format
/// for wildcard positions. `@` is also forbidden so the CLI's `--bind KEY@VALUE`
/// parser stays unambiguous.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Segment(String);

impl Segment {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reserved single-character segment placed at the `link_id` wire slot when
    /// a producer is run without `--link-id`. User-supplied input cannot
    /// construct this segment via `try_from` (the reserved-sentinel rule
    /// rejects `_`); the runtime uses this constructor to materialize the
    /// default sentinel at producer-side fan-out sites.
    pub fn default_link_id() -> Self {
        Self(DEFAULT_LINK_ID.to_string())
    }

    /// Lenient parser used by runtime paths that need to accept the reserved
    /// link_id default `_` alongside user-supplied link_ids (for example, when
    /// parsing the `PEPPY_LINK_IDS` env var inside the runner). Continues to
    /// reject `*` / `**` so a publisher can never advertise itself on a
    /// wildcard.
    pub fn try_link_id(s: &str) -> Result<Self, SegmentError> {
        validate_segment_chars(s)?;
        if matches!(s, "*" | "**") {
            return Err(SegmentError::ReservedSentinel(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    /// `Some(value)` → [`Self::try_link_id`]; `None` →
    /// [`Self::default_link_id`]. Used by wire constructors to map the
    /// optional consumer/producer `link_id` argument.
    pub fn link_id_or_default(value: Option<&str>) -> Result<Self, SegmentError> {
        match value {
            Some(s) => Self::try_link_id(s),
            None => Ok(Self::default_link_id()),
        }
    }
}

/// Wire literal used at the `link_id` slot when a producer is run without
/// `--link-id`. Kept in sync with [`Segment::default_link_id`] / the CLI
/// validator in `peppy::commands::node::run`. The single source of truth
/// lives in `config::consts::DEFAULT_LINK_ID_SENTINEL`; this is a re-export.
pub const DEFAULT_LINK_ID: &str = config::consts::DEFAULT_LINK_ID_SENTINEL;

impl std::ops::Deref for Segment {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Segment {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Segment {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl TryFrom<&str> for Segment {
    type Error = SegmentError;

    fn try_from(s: &str) -> Result<Self, SegmentError> {
        validate_segment_chars(s)?;
        if matches!(s, "*" | "**") || s == DEFAULT_LINK_ID {
            return Err(SegmentError::ReservedSentinel(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }
}

/// Shared char-level validation: non-empty, no `/`, no `@`. The reserved
/// sentinel check differs between [`Segment::try_link_id`] (rejects only
/// `*`/`**`) and [`Segment::try_from`] (also rejects `_`), so callers
/// apply that check separately.
fn validate_segment_chars(s: &str) -> Result<(), SegmentError> {
    if s.is_empty() {
        return Err(SegmentError::Empty);
    }
    if s.contains('/') {
        return Err(SegmentError::ContainsSlash(s.to_string()));
    }
    if s.contains('@') {
        return Err(SegmentError::ContainsAt(s.to_string()));
    }
    Ok(())
}

impl TryFrom<String> for Segment {
    type Error = SegmentError;

    fn try_from(s: String) -> Result<Self, SegmentError> {
        Self::try_from(s.as_str())
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returned by [`Segment::try_from`] when a candidate string violates the
/// keyexpr-segment invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
    Empty,
    ContainsSlash(String),
    ContainsAt(String),
    ReservedSentinel(String),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("keyexpr segment must not be empty"),
            Self::ContainsSlash(s) => {
                write!(f, "keyexpr segment '{s}' must not contain '/'")
            }
            Self::ContainsAt(s) => {
                write!(f, "keyexpr segment '{s}' must not contain '@'")
            }
            Self::ReservedSentinel(s) => {
                write!(f, "keyexpr segment '{s}' collides with a reserved sentinel")
            }
        }
    }
}

impl std::error::Error for SegmentError {}

/// Wire discriminator placed before the name/tag pair on senders whose target
/// is an interface (a `conforms_to`-bearing declaration).
pub(crate) const INTERFACE_DISCRIMINATOR: &str = "interface";

/// Wire discriminator placed before the name/tag pair on senders whose target
/// is a node (no `conforms_to`).
pub(crate) const NODE_DISCRIMINATOR: &str = "node";

/// Hyphen-to-underscore normalization applied at construction time to any tag
/// segment. The generator emits tags with hyphens (config-side identifier rule);
/// the wire form requires identifier-safe segments.
fn normalize_tag(tag: &str) -> String {
    if tag.contains('-') {
        tag.replace('-', "_")
    } else {
        tag.to_string()
    }
}

fn validated_name_tag(name: &str, tag: &str) -> Result<(Segment, Segment), SenderTargetError> {
    let name_segment = Segment::try_from(name)?;
    let normalized_tag = normalize_tag(tag);
    let tag_segment = Segment::try_from(normalized_tag.as_str())?;
    Ok((name_segment, tag_segment))
}

/// Identifier of an interface declared via `conforms_to`. Carries the
/// interface's name and tag. Used as one variant of [`SenderTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceIdentifier {
    interface_name: Segment,
    interface_tag: Segment,
}

impl InterfaceIdentifier {
    pub fn new(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        let (interface_name, interface_tag) = validated_name_tag(name, tag)?;
        Ok(Self {
            interface_name,
            interface_tag,
        })
    }

    pub fn name(&self) -> &str {
        self.interface_name.as_str()
    }

    pub fn tag(&self) -> &str {
        self.interface_tag.as_str()
    }
}

/// Identifier of a node (no `conforms_to`). Carries the node's name and tag
/// (from `manifest.tag`). Used as one variant of [`SenderTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeIdentifier {
    node_name: Segment,
    node_tag: Segment,
}

impl NodeIdentifier {
    pub fn new(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        let (node_name, node_tag) = validated_name_tag(name, tag)?;
        Ok(Self {
            node_name,
            node_tag,
        })
    }

    /// Builds a node identifier from segments the caller has already validated
    /// upstream (e.g. via `config::Name`). Panics if a segment turns out to
    /// collide with a reserved wire sentinel; the only inputs that can trigger
    /// this are degenerate `Name`s like `"_"` or `"-"` (the latter after
    /// hyphen-to-underscore tag normalization). Use this at call sites whose
    /// inputs were funneled through a typed `Name` boundary.
    pub fn from_validated(name: &str, tag: &str) -> Self {
        Self::new(name, tag).expect("validated name and tag should be wire-segment safe")
    }

    pub fn name(&self) -> &str {
        self.node_name.as_str()
    }

    pub fn tag(&self) -> &str {
        self.node_tag.as_str()
    }
}

/// Addressing target carried by a sender (or matched by a receiver). Each
/// emission is **either** an interface **or** a node, never both. The wire
/// format embeds an `interface`|`node` discriminator so the two identifier
/// spaces cannot collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SenderTarget {
    Interface(InterfaceIdentifier),
    Node(NodeIdentifier),
}

impl SenderTarget {
    /// Shortcut: `SenderTarget::interface("manipulator", "v1")` instead of
    /// `SenderTarget::Interface(InterfaceIdentifier::new("manipulator", "v1")?)`.
    pub fn interface(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        InterfaceIdentifier::new(name, tag).map(Self::Interface)
    }

    /// Shortcut: `SenderTarget::node("uvc_camera", "v1")` instead of
    /// `SenderTarget::Node(NodeIdentifier::new("uvc_camera", "v1")?)`.
    pub fn node(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        NodeIdentifier::new(name, tag).map(Self::Node)
    }

    /// Builds a node-shaped target from segments validated upstream (e.g. via
    /// `config::Name`). See [`NodeIdentifier::from_validated`] for the panic
    /// contract.
    pub fn node_from_validated(name: &str, tag: &str) -> Self {
        Self::Node(NodeIdentifier::from_validated(name, tag))
    }

    pub(crate) fn discriminator(&self) -> &'static str {
        match self {
            Self::Interface(_) => INTERFACE_DISCRIMINATOR,
            Self::Node(_) => NODE_DISCRIMINATOR,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Interface(i) => i.name(),
            Self::Node(n) => n.name(),
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::Interface(i) => i.tag(),
            Self::Node(n) => n.tag(),
        }
    }

    pub fn is_interface(&self) -> bool {
        matches!(self, Self::Interface(_))
    }

    pub fn is_node(&self) -> bool {
        matches!(self, Self::Node(_))
    }
}

/// Returned by [`InterfaceIdentifier::new`] / [`NodeIdentifier::new`] /
/// [`SenderTarget::interface`] / [`SenderTarget::node`] when a name or tag
/// segment fails validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderTargetError {
    InvalidSegment(SegmentError),
}

impl From<SegmentError> for SenderTargetError {
    fn from(err: SegmentError) -> Self {
        Self::InvalidSegment(err)
    }
}

impl fmt::Display for SenderTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSegment(err) => write!(f, "invalid sender target segment: {err}"),
        }
    }
}

impl std::error::Error for SenderTargetError {}

/// Discriminator for service-shaped traffic. Replaces the stringly-typed
/// `message_type: &str` (`"service"` / `"action"`) parameter previously
/// threaded through call sites.
///
/// On the wire, `Service` produces `service/{discriminator}/.../{name}` while
/// action variants produce `action/{discriminator}/.../{name}/{goal|cancel|result}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    Service,
    ActionGoal,
    ActionCancel,
    ActionResult,
}

impl ServiceKind {
    /// First segment of the service root (`"service"` or `"action"`).
    pub fn root_segment(self) -> &'static str {
        match self {
            ServiceKind::Service => "service",
            ServiceKind::ActionGoal | ServiceKind::ActionCancel | ServiceKind::ActionResult => {
                "action"
            }
        }
    }

    /// Trailing segment appended after the service name for action sub-services,
    /// or `None` for a plain service.
    pub fn suffix(self) -> Option<&'static str> {
        match self {
            ServiceKind::Service => None,
            ServiceKind::ActionGoal => Some("goal"),
            ServiceKind::ActionCancel => Some("cancel"),
            ServiceKind::ActionResult => Some("result"),
        }
    }
}

/// The precomputed `[root, discriminator, name, tag]` prefix segments of a
/// service_root. Built once at receiver construction so the inbound-query
/// parser can match without rebuilding strings. Single source of truth for the
/// prefix shape, shared by [`ServiceWireReceiver::new`] and the action
/// sub-service derivation in [`ActionWireReceiver`].
fn service_root_prefix(identity: &SenderTarget, kind: ServiceKind) -> [String; 4] {
    [
        kind.root_segment().to_string(),
        identity.discriminator().to_string(),
        identity.name().to_string(),
        identity.tag().to_string(),
    ]
}

// ─── Topics ──────────────────────────────────────────────────────────────────

/// Publisher-side addressing for a topic emit. Fields are `pub(crate)` so
/// external callers go through the validating [`Self::new`] constructor; the
/// wire format and adapter code inside this crate can read fields directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_target: SenderTarget,
    pub(crate) link_id: Segment,
    pub(crate) as_topic_name: Segment,
}

impl TopicWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        as_topic_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_target,
            link_id: Segment::link_id_or_default(link_id)?,
            as_topic_name: Segment::try_from(as_topic_name)?,
        })
    }
}

/// Subscriber-side addressing for a topic. `from_core_node` / `from_instance_id` /
/// `from_target` identify the publisher whose messages we want to receive;
/// `None` means "any" (translated to the transport's single-chunk wildcard).
/// `from_link_id` follows the same rule: `Some` pins to a producer's specific
/// link_id, `None` matches any (used for `from_any: true` consumers).
///
/// `defers_secondary_drop` tells the adapter that peppylib's `Subscription`
/// wrapper applies its own per-link_id filter above the adapter, so the
/// adapter must stop dropping the secondary publishes of a multi-link
/// `emit` (which is its default behavior for wildcard subscribers). Set by
/// peppylib whenever the consumer has registered sibling-pinned link_ids
/// for this `(name, tag)`; the actual filter values live on
/// [`crate::Subscription`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) from_core_node: Option<Segment>,
    pub(crate) from_instance_id: Option<Segment>,
    pub(crate) from_target: Option<SenderTarget>,
    pub(crate) from_link_id: Option<Segment>,
    pub(crate) to_topic: Segment,
    pub(crate) defers_secondary_drop: bool,
}

impl TopicWireReceiver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        from_target: Option<SenderTarget>,
        from_link_id: Option<&str>,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            from_core_node: from_core_node.map(Segment::try_from).transpose()?,
            from_instance_id: from_instance_id.map(Segment::try_from).transpose()?,
            from_target,
            from_link_id: from_link_id.map(Segment::try_link_id).transpose()?,
            to_topic: Segment::try_from(to_topic)?,
            defers_secondary_drop: false,
        })
    }

    /// Tells the adapter to skip its built-in secondary-publish drop because
    /// peppylib will apply a sibling-pinned link_id filter above it. Set to
    /// `true` only when the consumer's manifest registers sibling-pinned
    /// link_ids for this `(name, tag)`.
    pub fn with_defers_secondary_drop(mut self, defers: bool) -> Self {
        self.defers_secondary_drop = defers;
        self
    }
}

// ─── Services ────────────────────────────────────────────────────────────────

/// Caller-side addressing for a service. `target` is the producer's full
/// `(core_node, instance_id)` wire address; `None` is a genuine wildcard
/// on both slots (the discovery probe shape). The one representable
/// half-address is core-without-instance via [`Self::scoped_to_core_node`]
/// — for callers that know which core node must answer but cannot know
/// the producer's per-boot instance_id. Instance-without-core stays
/// unrepresentable at this boundary.
/// The link_id wire slot is always emitted as `*` — producers advertise under
/// the reserved `_` segment, and Zenoh's matcher unifies the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireSender {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) target_core_node: Option<Segment>,
    pub(crate) target_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireSender {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        target: Option<&ProducerRef>,
        to_target: SenderTarget,
        to_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            target_core_node: target
                .map(|t| Segment::try_from(t.core_node.as_str()))
                .transpose()?,
            target_instance_id: target
                .map(|t| Segment::try_from(t.instance_id.as_str()))
                .transpose()?,
            to_target,
            to_service_name: Segment::try_from(to_service_name)?,
            kind,
        })
    }

    /// Scopes the selector's target core_node slot to `target_core_node`
    /// while leaving the instance slot wildcarded — the core-without-instance
    /// half-address. The selector then matches only producers hosted by that
    /// core node, regardless of their per-boot instance_id.
    pub fn scoped_to_core_node(mut self, target_core_node: &str) -> crate::error::Result<Self> {
        self.target_core_node = Some(Segment::try_from(target_core_node)?);
        self.target_instance_id = None;
        Ok(self)
    }

    pub fn to_service_name(&self) -> &str {
        &self.to_service_name
    }

    pub fn target_instance_id(&self) -> Option<&str> {
        self.target_instance_id.as_deref()
    }
}

/// Server-side addressing for a service. Producers always advertise their
/// queryables under the reserved default `_` segment at the link_id wire
/// slot; inbound queries carry either `*` (from `from_any` consumers) or
/// `_`, and the dispatch filter at the adapter accepts both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireReceiver {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_identity: SenderTarget,
    pub(crate) as_service_name: Segment,
    pub(crate) kind: ServiceKind,
    /// Precomputed `[root, discriminator, name, tag]` segments of the
    /// service_root prefix. Derived from `(as_identity, kind)` at construction
    /// so the inbound query parser can match without rebuilding strings.
    service_root_prefix: [String; 4],
}

impl ServiceWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        let service_root_prefix = service_root_prefix(&as_identity, kind);
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_identity,
            as_service_name: Segment::try_from(as_service_name)?,
            kind,
            service_root_prefix,
        })
    }

    pub(crate) fn service_root_prefix_segments(&self) -> &[String; 4] {
        &self.service_root_prefix
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Caller-side addressing for an action. Goal / cancel / result are exposed
/// as derived [`ServiceWireSender`]s with the appropriate [`ServiceKind`].
/// Feedback subscription is built per `goal_id` by the transport adapter.
/// `target` is the producer's full `(core_node, instance_id)` wire address;
/// `None` is a genuine wildcard on both slots (the discovery probe shape).
/// The link_id wire slot is always `*` — producers advertise under the
/// reserved `_` segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) target_core_node: Option<Segment>,
    pub(crate) target_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_action_name: Segment,
}

impl ActionWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        target: Option<&ProducerRef>,
        to_target: SenderTarget,
        to_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            target_core_node: target
                .map(|t| Segment::try_from(t.core_node.as_str()))
                .transpose()?,
            target_instance_id: target
                .map(|t| Segment::try_from(t.instance_id.as_str()))
                .transpose()?,
            to_target,
            to_action_name: Segment::try_from(to_action_name)?,
        })
    }

    pub fn goal_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionGoal)
    }

    pub fn cancel_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionCancel)
    }

    pub fn result_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionResult)
    }

    pub fn to_action_name(&self) -> &str {
        &self.to_action_name
    }

    pub fn target_core_node(&self) -> Option<&str> {
        self.target_core_node.as_deref()
    }

    pub fn target_instance_id(&self) -> Option<&str> {
        self.target_instance_id.as_deref()
    }

    /// Returns a clone with `target_core_node` and `target_instance_id`
    /// overwritten by the given values. Used by `ActionMessenger::send_goal`
    /// to latch the stored sender to the responding producer after the first
    /// `goal_response` arrives, so cancel / result / feedback all target the
    /// winner instead of fanning out to every producer that received the
    /// wildcard goal.
    pub fn pinned_to(&self, core_node: &str, instance_id: &str) -> crate::error::Result<Self> {
        let mut out = self.clone();
        out.target_core_node = Some(Segment::try_from(core_node)?);
        out.target_instance_id = Some(Segment::try_from(instance_id)?);
        Ok(out)
    }

    fn action_service(&self, kind: ServiceKind) -> ServiceWireSender {
        ServiceWireSender {
            bound_core_node: self.as_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            target_core_node: self.target_core_node.clone(),
            target_instance_id: self.target_instance_id.clone(),
            to_target: self.to_target.clone(),
            to_service_name: self.to_action_name.clone(),
            kind,
        }
    }
}

/// Server-side addressing for an action. Producers always advertise under the
/// reserved default `_` link_id segment; the adapter accepts `*` or `_` at the
/// link_id wire slot. Per-goal feedback publishes use the goal's own link_id
/// (extracted from the goal request).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireReceiver {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_identity: SenderTarget,
    pub(crate) as_action_name: Segment,
}

impl ActionWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_identity,
            as_action_name: Segment::try_from(as_action_name)?,
        })
    }

    pub fn goal_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionGoal)
    }

    pub fn cancel_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionCancel)
    }

    pub fn result_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionResult)
    }

    fn action_service(&self, kind: ServiceKind) -> ServiceWireReceiver {
        let service_root_prefix = service_root_prefix(&self.as_identity, kind);
        ServiceWireReceiver {
            bound_core_node: self.bound_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            as_identity: self.as_identity.clone(),
            as_service_name: self.as_action_name.clone(),
            kind,
            service_root_prefix,
        }
    }
}

pub(crate) mod zenoh_format;

pub use zenoh_format::{ServiceQueryKind, ServiceReplyKind};

#[cfg(test)]
mod tests;
