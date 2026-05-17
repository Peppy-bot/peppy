//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / iface / topic|service|action / name) is
//! peppy-specific; the wire format that encodes it is transport-specific and lives
//! in the per-transport adapter modules (e.g. `adapters::zenoh_wire`).

use std::fmt;

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
    Conformed { name: String, tag: String },
}

impl Iface {
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self::Conformed {
            name: name.into(),
            tag: Self::normalize_tag(tag.into()),
        }
    }

    pub const fn native() -> Self {
        Self::Native
    }

    pub const fn wildcard() -> Self {
        Self::Wildcard
    }

    /// `(None, None)` → native; `(Some, Some)` → conformed iface; one-side
    /// → `IfaceError`. Enforces the pair invariant at the construction
    /// boundary so the rest of the code never has to.
    pub fn from_options(name: Option<&str>, tag: Option<&str>) -> Result<Self, IfaceError> {
        match (name, tag) {
            (None, None) => Ok(Self::native()),
            (Some(n), Some(t)) => Ok(Self::new(n, t)),
            (Some(_), None) | (None, Some(_)) => Err(IfaceError),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Native => NATIVE_IFACE_SEGMENT,
            Self::Wildcard => WILDCARD_IFACE_SEGMENT,
            Self::Conformed { name, .. } => name,
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::Native => NATIVE_IFACE_SEGMENT,
            Self::Wildcard => WILDCARD_IFACE_SEGMENT,
            Self::Conformed { tag, .. } => tag,
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    fn normalize_tag(tag: String) -> String {
        if tag.contains('-') {
            tag.replace('-', "_")
        } else {
            tag
        }
    }
}

/// Returned by [`Iface::from_options`] when exactly one of `name` / `tag` is
/// `Some`. Both must be paired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfaceError;

impl fmt::Display for IfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("iface_name and iface_tag must both be set or both be None")
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

/// Publisher-side addressing for a topic emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireSender {
    pub as_core_node: String,
    pub as_instance_id: String,
    pub as_node_name: String,
    pub iface: Iface,
    pub as_topic_name: String,
}

impl TopicWireSender {
    pub fn new(
        as_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        as_node_name: impl Into<String>,
        iface: Iface,
        as_topic_name: impl Into<String>,
    ) -> Self {
        Self {
            as_core_node: as_core_node.into(),
            as_instance_id: as_instance_id.into(),
            as_node_name: as_node_name.into(),
            iface,
            as_topic_name: as_topic_name.into(),
        }
    }
}

/// Subscriber-side addressing for a topic. `to_core_node` / `to_instance_id` /
/// `to_node_name` are `None` to mean "any" (translated to the transport's
/// single-chunk wildcard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub as_core_node: String,
    pub as_instance_id: String,
    pub to_core_node: Option<String>,
    pub to_instance_id: Option<String>,
    pub to_node_name: Option<String>,
    pub iface: Iface,
    pub to_topic: String,
}

impl TopicWireReceiver {
    pub fn new(
        as_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_node_name: Option<&str>,
        iface: Iface,
        to_topic: impl Into<String>,
    ) -> Self {
        Self {
            as_core_node: as_core_node.into(),
            as_instance_id: as_instance_id.into(),
            to_core_node: to_core_node.map(str::to_string),
            to_instance_id: to_instance_id.map(str::to_string),
            to_node_name: to_node_name.map(str::to_string),
            iface,
            to_topic: to_topic.into(),
        }
    }
}

// ─── Services ────────────────────────────────────────────────────────────────

/// Caller-side addressing for a service. `to_core_node` / `to_instance_id`
/// are `None` for broadcast (translated to the protocol's `_any_` marker).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireSender {
    pub bound_core_node: String,
    pub as_instance_id: String,
    pub to_core_node: Option<String>,
    pub to_instance_id: Option<String>,
    pub to_node_name: String,
    pub iface: Iface,
    pub to_service_name: String,
    pub kind: ServiceKind,
}

impl ServiceWireSender {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bound_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_node_name: impl Into<String>,
        iface: Iface,
        to_service_name: impl Into<String>,
        kind: ServiceKind,
    ) -> Self {
        Self {
            bound_core_node: bound_core_node.into(),
            as_instance_id: as_instance_id.into(),
            to_core_node: to_core_node.map(str::to_string),
            to_instance_id: to_instance_id.map(str::to_string),
            to_node_name: to_node_name.into(),
            iface,
            to_service_name: to_service_name.into(),
            kind,
        }
    }
}

/// Server-side addressing for a service. The four broadcast-Cartesian listen
/// patterns are derived from this single context by the transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireReceiver {
    pub bound_core_node: String,
    pub as_instance_id: String,
    pub as_node_name: String,
    pub iface: Iface,
    pub as_service_name: String,
    pub kind: ServiceKind,
}

impl ServiceWireReceiver {
    pub fn new(
        bound_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        as_node_name: impl Into<String>,
        iface: Iface,
        as_service_name: impl Into<String>,
        kind: ServiceKind,
    ) -> Self {
        Self {
            bound_core_node: bound_core_node.into(),
            as_instance_id: as_instance_id.into(),
            as_node_name: as_node_name.into(),
            iface,
            as_service_name: as_service_name.into(),
            kind,
        }
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Caller-side addressing for an action. Goal / cancel / result are exposed
/// as derived [`ServiceWireSender`]s with the appropriate [`ServiceKind`].
/// Feedback subscription is built per `goal_id` by the transport adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireSender {
    pub as_core_node: String,
    pub as_instance_id: String,
    pub to_core_node: Option<String>,
    pub to_instance_id: Option<String>,
    pub to_node_name: String,
    pub iface: Iface,
    pub to_action_name: String,
}

impl ActionWireSender {
    pub fn new(
        as_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_node_name: impl Into<String>,
        iface: Iface,
        to_action_name: impl Into<String>,
    ) -> Self {
        Self {
            as_core_node: as_core_node.into(),
            as_instance_id: as_instance_id.into(),
            to_core_node: to_core_node.map(str::to_string),
            to_instance_id: to_instance_id.map(str::to_string),
            to_node_name: to_node_name.into(),
            iface,
            to_action_name: to_action_name.into(),
        }
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
    pub bound_core_node: String,
    pub as_instance_id: String,
    pub as_node_name: String,
    pub iface: Iface,
    pub as_action_name: String,
}

impl ActionWireReceiver {
    pub fn new(
        bound_core_node: impl Into<String>,
        as_instance_id: impl Into<String>,
        as_node_name: impl Into<String>,
        iface: Iface,
        as_action_name: impl Into<String>,
    ) -> Self {
        Self {
            bound_core_node: bound_core_node.into(),
            as_instance_id: as_instance_id.into(),
            as_node_name: as_node_name.into(),
            iface,
            as_action_name: as_action_name.into(),
        }
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

#[cfg(test)]
mod tests;
