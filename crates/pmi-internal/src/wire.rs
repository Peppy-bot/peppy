//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / iface / topic|service|action / name) is
//! peppy-specific; the wire format that encodes it is transport-specific and lives
//! in the per-transport adapter modules (e.g. `adapters::zenoh_wire`).

use std::fmt;

/// Wire segment used for the iface_name and iface_tag positions when an
/// artifact is native (not part of a `conforms_to` interface).
pub const NATIVE_IFACE_SEGMENT: &str = "_";

/// Wire marker used to indicate "any" target in broadcast routing. Distinct
/// from zenoh's `*` wildcard — broadcasts are explicit at the protocol level.
pub const BROADCAST_MARKER: &str = "_any_";

/// Paired `iface_name` + `iface_tag`. Either both come from a `conforms_to`
/// interface or both are the native sentinel — never one without the other.
/// Tag is hyphen-normalized once at construction (the generator emits tags
/// with hyphens; the wire form requires identifiers).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Iface {
    name: String,
    tag: String,
}

impl Iface {
    pub fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: Self::normalize_tag(tag.into()),
        }
    }

    pub fn native() -> Self {
        Self {
            name: NATIVE_IFACE_SEGMENT.to_string(),
            tag: NATIVE_IFACE_SEGMENT.to_string(),
        }
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
        &self.name
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn is_native(&self) -> bool {
        self.name == NATIVE_IFACE_SEGMENT && self.tag == NATIVE_IFACE_SEGMENT
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

/// Subscriber-side addressing for a topic. `to_core_node` / `to_instance_id`
/// are `None` to mean "any" (translated to the transport's single-chunk
/// wildcard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub as_core_node: String,
    pub as_instance_id: String,
    pub to_core_node: Option<String>,
    pub to_instance_id: Option<String>,
    pub to_node_name: String,
    pub iface: Iface,
    pub to_topic: String,
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
