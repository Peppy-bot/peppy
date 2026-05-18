//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / iface / topic|service|action / name) is
//! peppy-specific. The zenoh-shaped wire format that encodes it lives in
//! `wire::zenoh_format`.

use std::fmt;

/// A validated keyexpr segment. The wire format builds keyexprs by joining
/// segments with `/`, so a segment must be non-empty, contain no `/`, and not
/// collide with the reserved sentinels (`*`, `**`, `_`) used by [`Iface`] and
/// the wire format for wildcard / native positions.
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

/// Wire segment used for the iface_name and iface_tag positions when an
/// artifact is native (not part of a `conforms_to` interface).
pub(crate) const NATIVE_IFACE_SEGMENT: &str = "_";

/// Wire segment used when an iface position should match any value. Coincides
/// with zenoh's single-chunk wildcard so the segment can be embedded verbatim
/// in keyexprs by the zenoh adapter.
pub(crate) const WILDCARD_IFACE_SEGMENT: &str = "*";

/// Wire marker used to indicate "any" target in broadcast routing. Distinct
/// from a transport-level wildcard: broadcasts are explicit at the protocol
/// level so the responder can decide whether to answer.
pub(crate) const BROADCAST_MARKER: &str = "_any_";

/// Paired iface segments. `Native` corresponds to an artifact that is not part
/// of a `conforms_to` interface; `Wildcard` matches any iface for external
/// consumers that don't know the producer's identity; `Conformed` carries the
/// interface name and tag from a `conforms_to` declaration.
///
/// Tag is hyphen-normalized once at construction (the generator emits tags
/// with hyphens; the wire form requires identifiers).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Iface {
    Native,
    Wildcard,
    Conformed { name: Segment, tag: Segment },
}

impl Iface {
    pub fn new(name: &str, tag: &str) -> Result<Self, IfaceError> {
        let name = Segment::try_from(name)?;
        let normalized_tag = Self::normalize_tag(tag);
        let tag = Segment::try_from(normalized_tag.as_str())?;
        Ok(Self::Conformed { name, tag })
    }

    pub const fn native() -> Self {
        Self::Native
    }

    pub const fn wildcard() -> Self {
        Self::Wildcard
    }

    /// `(None, None)` → native; `(Some, Some)` → conformed iface; one-side
    /// → `IfaceError::UnpairedOptions`. Enforces the pair invariant at the
    /// construction boundary so the rest of the code never has to.
    pub fn from_options(name: Option<&str>, tag: Option<&str>) -> Result<Self, IfaceError> {
        match (name, tag) {
            (None, None) => Ok(Self::native()),
            (Some(n), Some(t)) => Self::new(n, t),
            (Some(_), None) | (None, Some(_)) => Err(IfaceError::UnpairedOptions),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Native => NATIVE_IFACE_SEGMENT,
            Self::Wildcard => WILDCARD_IFACE_SEGMENT,
            Self::Conformed { name, .. } => name.as_str(),
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::Native => NATIVE_IFACE_SEGMENT,
            Self::Wildcard => WILDCARD_IFACE_SEGMENT,
            Self::Conformed { tag, .. } => tag.as_str(),
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    fn normalize_tag(tag: &str) -> String {
        if tag.contains('-') {
            tag.replace('-', "_")
        } else {
            tag.to_string()
        }
    }
}

/// Returned by [`Iface::new`] and [`Iface::from_options`].
///
/// `UnpairedOptions` is reported only by `from_options` when exactly one of
/// `name` / `tag` is `Some`. `InvalidSegment` wraps the per-segment validation
/// failure when either input is not a valid [`Segment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfaceError {
    UnpairedOptions,
    InvalidSegment(SegmentError),
}

impl From<SegmentError> for IfaceError {
    fn from(err: SegmentError) -> Self {
        Self::InvalidSegment(err)
    }
}

impl fmt::Display for IfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnpairedOptions => {
                f.write_str("iface_name and iface_tag must both be set or both be None")
            }
            Self::InvalidSegment(err) => write!(f, "invalid iface segment: {err}"),
        }
    }
}

impl std::error::Error for IfaceError {}

/// Discriminator for service-shaped traffic. Replaces the stringly-typed
/// `message_type: &str` (`"service"` / `"action"`) parameter previously
/// threaded through call sites.
///
/// On the wire, `Service` produces `service/{node}/.../{name}` while action
/// variants produce `action/{node}/.../{name}/{goal|cancel|result}`.
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
    pub(crate) as_node_name: Segment,
    pub(crate) iface: Iface,
    pub(crate) as_topic_name: Segment,
}

impl TopicWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface: Iface,
        as_topic_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_node_name: Segment::try_from(as_node_name)?,
            iface,
            as_topic_name: Segment::try_from(as_topic_name)?,
        })
    }
}

/// Subscriber-side addressing for a topic. `from_core_node` / `from_instance_id` /
/// `from_node_name` identify the publisher whose messages we want to receive;
/// `None` means "any" (translated to the transport's single-chunk wildcard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) from_core_node: Option<Segment>,
    pub(crate) from_instance_id: Option<Segment>,
    pub(crate) from_node_name: Option<Segment>,
    pub(crate) iface: Iface,
    pub(crate) to_topic: Segment,
}

impl TopicWireReceiver {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        from_node_name: Option<&str>,
        iface: Iface,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            from_core_node: from_core_node.map(Segment::try_from).transpose()?,
            from_instance_id: from_instance_id.map(Segment::try_from).transpose()?,
            from_node_name: from_node_name.map(Segment::try_from).transpose()?,
            iface,
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
    pub(crate) to_node_name: Segment,
    pub(crate) iface: Iface,
    pub(crate) to_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireSender {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_node_name: &str,
        iface: Iface,
        to_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            to_core_node: to_core_node.map(Segment::try_from).transpose()?,
            to_instance_id: to_instance_id.map(Segment::try_from).transpose()?,
            to_node_name: Segment::try_from(to_node_name)?,
            iface,
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
    pub(crate) as_node_name: Segment,
    pub(crate) iface: Iface,
    pub(crate) as_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface: Iface,
        as_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_node_name: Segment::try_from(as_node_name)?,
            iface,
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
    pub(crate) to_node_name: Segment,
    pub(crate) iface: Iface,
    pub(crate) to_action_name: Segment,
}

impl ActionWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_node_name: &str,
        iface: Iface,
        to_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            to_core_node: to_core_node.map(Segment::try_from).transpose()?,
            to_instance_id: to_instance_id.map(Segment::try_from).transpose()?,
            to_node_name: Segment::try_from(to_node_name)?,
            iface,
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
            to_node_name: self.to_node_name.clone(),
            iface: self.iface.clone(),
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
    pub(crate) as_node_name: Segment,
    pub(crate) iface: Iface,
    pub(crate) as_action_name: Segment,
}

impl ActionWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface: Iface,
        as_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_node_name: Segment::try_from(as_node_name)?,
            iface,
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
            as_node_name: self.as_node_name.clone(),
            iface: self.iface.clone(),
            as_service_name: self.as_action_name.clone(),
            kind,
        }
    }
}

pub(crate) mod zenoh_format;

#[cfg(test)]
mod tests;
