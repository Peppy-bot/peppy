//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / target / topic|service|action / name)
//! is peppy-specific. The zenoh-shaped wire format that encodes it lives in
//! `wire::zenoh_format`.

use std::fmt;

/// A validated keyexpr segment. The wire format builds keyexprs by joining
/// segments with `/`, so a segment must be non-empty, contain no `/`, and not
/// collide with the reserved sentinels (`*`, `**`, `_`) used by the wire format
/// for wildcard positions.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Segment(String);

impl Segment {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
        if s.is_empty() {
            return Err(SegmentError::Empty);
        }
        if s.contains('/') {
            return Err(SegmentError::ContainsSlash(s.to_string()));
        }
        if matches!(s, "*" | "**" | "_") {
            return Err(SegmentError::ReservedSentinel(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }
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
    ReservedSentinel(String),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("keyexpr segment must not be empty"),
            Self::ContainsSlash(s) => {
                write!(f, "keyexpr segment '{s}' must not contain '/'")
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

/// Wire marker used to indicate "any" target in broadcast routing. Distinct
/// from a transport-level wildcard: broadcasts are explicit at the protocol
/// level so the responder can decide whether to answer.
pub(crate) const BROADCAST_MARKER: &str = "_any_";

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

// ─── Topics ──────────────────────────────────────────────────────────────────

/// Publisher-side addressing for a topic emit. Fields are `pub(crate)` so
/// external callers go through the validating [`Self::new`] constructor; the
/// wire format and adapter code inside this crate can read fields directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_target: SenderTarget,
    pub(crate) as_topic_name: Segment,
}

impl TopicWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        as_topic_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_target,
            as_topic_name: Segment::try_from(as_topic_name)?,
        })
    }
}

/// Subscriber-side addressing for a topic. `from_core_node` / `from_instance_id` /
/// `from_target` identify the publisher whose messages we want to receive;
/// `None` means "any" (translated to the transport's single-chunk wildcard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) from_core_node: Option<Segment>,
    pub(crate) from_instance_id: Option<Segment>,
    pub(crate) from_target: Option<SenderTarget>,
    pub(crate) to_topic: Segment,
}

impl TopicWireReceiver {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        from_target: Option<SenderTarget>,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            from_core_node: from_core_node.map(Segment::try_from).transpose()?,
            from_instance_id: from_instance_id.map(Segment::try_from).transpose()?,
            from_target,
            to_topic: Segment::try_from(to_topic)?,
        })
    }
}

// ─── Services ────────────────────────────────────────────────────────────────

/// Caller-side addressing for a service. `to_core_node` / `to_instance_id`
/// are `None` for broadcast (translated to the protocol's `_any_` marker).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireSender {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) to_core_node: Option<Segment>,
    pub(crate) to_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireSender {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_target: SenderTarget,
        to_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            to_core_node: to_core_node.map(Segment::try_from).transpose()?,
            to_instance_id: to_instance_id.map(Segment::try_from).transpose()?,
            to_target,
            to_service_name: Segment::try_from(to_service_name)?,
            kind,
        })
    }

    pub fn to_service_name(&self) -> &str {
        &self.to_service_name
    }

    pub fn to_instance_id(&self) -> Option<&str> {
        self.to_instance_id.as_deref()
    }
}

/// Server-side addressing for a service. The four broadcast-Cartesian listen
/// patterns are derived from this single context by the transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireReceiver {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_identity: SenderTarget,
    pub(crate) as_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_identity,
            as_service_name: Segment::try_from(as_service_name)?,
            kind,
        })
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Caller-side addressing for an action. Goal / cancel / result are exposed
/// as derived [`ServiceWireSender`]s with the appropriate [`ServiceKind`].
/// Feedback subscription is built per `goal_id` by the transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) to_core_node: Option<Segment>,
    pub(crate) to_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_action_name: Segment,
}

impl ActionWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_target: SenderTarget,
        to_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            to_core_node: to_core_node.map(Segment::try_from).transpose()?,
            to_instance_id: to_instance_id.map(Segment::try_from).transpose()?,
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

    fn action_service(&self, kind: ServiceKind) -> ServiceWireSender {
        ServiceWireSender {
            bound_core_node: self.as_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            to_core_node: self.to_core_node.clone(),
            to_instance_id: self.to_instance_id.clone(),
            to_target: self.to_target.clone(),
            to_service_name: self.to_action_name.clone(),
            kind,
        }
    }
}

/// Server-side addressing for an action.
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
        ServiceWireReceiver {
            bound_core_node: self.bound_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            as_identity: self.as_identity.clone(),
            as_service_name: self.as_action_name.clone(),
            kind,
        }
    }
}

pub(crate) mod zenoh_format;

#[cfg(test)]
mod tests;
