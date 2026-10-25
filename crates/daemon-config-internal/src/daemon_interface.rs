//! The daemon interfaces this peppy serves: the registry that the exposure
//! checks, the derived node, the hub check, the daemon and its clients read.
//!
//! A daemon interface is a document in the contract format, compiled into
//! peppy. No node implements it: the peppy daemon serves it on the messaging
//! layer, as a provider of the contract, and checks each call against the
//! scope of its sender. peppy serves one tag of each interface. Each entry
//! of the registry holds the interface's document, the type of the scope a
//! launcher gives a target that names it, the narrowing of its published
//! schema by a scope, and the checks a launch runs on a scope.

pub mod fleet;
mod stack_copies;

use crate::internal::contract::{PeppyContract, PeppyContractParser};
use crate::internal::launcher::{JoinableNodes, PreparedLauncher, UnitSelection, VacantReason};
use peppy_mcp_catalog::{DaemonInterfaceRef, ExposureBundle, McpExposure, ResolvedInterface};
use std::sync::LazyLock;

use fleet::FleetScope;
pub use stack_copies::{
    Description, MAX_DESCRIPTION_CHARS, MAX_FEATURES, PlacedFeature, ScopedFeature, ScopedOption,
    StackCopiesMember, StackCopiesScope,
};

/// One daemon interface this peppy serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DaemonInterface {
    /// `stack_copies`: list the copies of the running launch, add one,
    /// remove one.
    StackCopies,
    /// `fleet`: the fleet listing and its changes, the leases of the robots,
    /// the move check and the halt of the stack.
    Fleet,
}

static STACK_COPIES_DOCUMENT: LazyLock<PeppyContract> = LazyLock::new(|| {
    PeppyContractParser::from_content(stack_copies::DOCUMENT)
        .expect("the compiled-in `stack_copies` document parses as a contract")
});

static FLEET_DOCUMENT: LazyLock<PeppyContract> = LazyLock::new(|| {
    PeppyContractParser::from_content(fleet::DOCUMENT)
        .expect("the compiled-in `fleet` document parses as a contract")
});

impl DaemonInterface {
    /// Every interface this peppy serves.
    pub const ALL: [Self; 2] = [Self::StackCopies, Self::Fleet];

    /// The name a target's `daemon` reference names the interface by.
    pub fn name(self) -> &'static str {
        match self {
            Self::StackCopies => "stack_copies",
            Self::Fleet => "fleet",
        }
    }

    /// The one tag of the interface this peppy serves.
    pub fn tag(self) -> &'static str {
        match self {
            Self::StackCopies | Self::Fleet => "v1",
        }
    }

    /// The interface as messages name it: `name:tag`.
    pub fn label(self) -> String {
        format!("{}:{}", self.name(), self.tag())
    }

    /// The interface's document in the contract format, as compiled in.
    pub fn document_text(self) -> &'static str {
        match self {
            Self::StackCopies => stack_copies::DOCUMENT,
            Self::Fleet => fleet::DOCUMENT,
        }
    }

    /// The interface's document, parsed.
    pub fn document(self) -> &'static PeppyContract {
        match self {
            Self::StackCopies => &STACK_COPIES_DOCUMENT,
            Self::Fleet => &FLEET_DOCUMENT,
        }
    }

    /// The interface as the catalog validates an exposure against it: its
    /// identity beside its members.
    pub fn resolved(self) -> ResolvedInterface<'static> {
        ResolvedInterface {
            name: self.name(),
            tag: self.tag(),
            members: self.document().declared_members(),
        }
    }

    /// The interface `reference` names, when this peppy serves it at the
    /// named tag.
    pub fn served(reference: &DaemonInterfaceRef) -> Result<Self, UnservedInterface> {
        let Some(interface) = Self::ALL
            .into_iter()
            .find(|interface| interface.name() == reference.name.as_str())
        else {
            return Err(UnservedInterface::UnknownName {
                interface: reference.to_string(),
                served: served_labels(),
            });
        };
        if interface.tag() != reference.tag {
            return Err(UnservedInterface::UnservedTag {
                interface: reference.to_string(),
                name: interface.name(),
                served: interface.tag(),
            });
        }
        Ok(interface)
    }

    /// Parses a scope a launcher gives a target that names this interface
    /// into the interface's scope type, which holds the scope's rules.
    pub fn parse_scope(self, value: &serde_json::Value) -> Result<DaemonScope, String> {
        match self {
            Self::StackCopies => {
                parse_value::<StackCopiesScope>(value).map(DaemonScope::StackCopies)
            }
            Self::Fleet => parse_value::<FleetScope>(value).map(DaemonScope::Fleet),
        }
    }
}

/// What a launcher gives one daemon target: a scope of the interface the
/// target names, or a vacancy, `{ vacant: "<why>" }`, which leaves the target
/// unserved: the server serves none of its entries, and the daemon grants
/// its client nothing through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetScope {
    Scoped(DaemonScope),
    Vacant(VacantReason),
}

impl DaemonInterface {
    /// Parses what a launcher gives a target that names this interface: a
    /// vacancy, or a scope of this interface ([`Self::parse_scope`]).
    pub fn parse_target_scope(self, value: &serde_json::Value) -> Result<TargetScope, String> {
        match vacancy(value)? {
            Some(reason) => Ok(TargetScope::Vacant(reason)),
            None => self.parse_scope(value).map(TargetScope::Scoped),
        }
    }
}

/// The reason of `value` when it is a vacancy, an object whose one key is
/// `vacant`; `None` for any other value.
fn vacancy(value: &serde_json::Value) -> Result<Option<VacantReason>, String> {
    let Some(fields) = value.as_object() else {
        return Ok(None);
    };
    let (Some(reason), 1) = (fields.get("vacant"), fields.len()) else {
        return Ok(None);
    };
    let Some(reason) = reason.as_str() else {
        return Err("`vacant` takes the reason the target stays unserved, as a string".to_owned());
    };
    VacantReason::new(reason)
        .map(Some)
        .map_err(|_| "`vacant` needs a reason: say why the target stays unserved".to_owned())
}

/// Every interface this peppy serves, as a refusal lists them.
fn served_labels() -> String {
    crate::error::format_quoted_list(
        DaemonInterface::ALL
            .iter()
            .map(|interface| interface.label()),
    )
}

/// Deserializes `value`, naming the field a refusal is about.
fn parse_value<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, String> {
    serde_path_to_error::deserialize(value.clone()).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        if path.is_empty() || path == "." {
            inner.to_string()
        } else {
            format!("{path}: {inner}")
        }
    })
}

/// Why this peppy does not serve the interface a daemon target names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnservedInterface {
    #[error("daemon interface `{interface}` is not one this peppy serves; it serves {served}")]
    UnknownName { interface: String, served: String },
    #[error(
        "daemon interface `{interface}` is not one this peppy serves; it serves `{name}` at tag \
         `{served}` only, and a document that names a daemon interface ships with the peppy \
         release that serves its tag"
    )]
    UnservedTag {
        interface: String,
        name: &'static str,
        served: &'static str,
    },
}

/// The daemon interface of each daemon target of `exposure`, resolved
/// through the registry, or one refusal per target that names an interface
/// this peppy does not serve.
pub fn served_interfaces(
    exposure: &McpExposure,
) -> Result<Vec<(String, DaemonInterface)>, Vec<String>> {
    let mut served = Vec::new();
    let mut refusals = Vec::new();
    for (target, selection, _) in exposure.surface.targets() {
        let Some(reference) = selection.source.daemon() else {
            continue;
        };
        match DaemonInterface::served(reference) {
            Ok(interface) => served.push((target.clone(), interface)),
            Err(error) => refusals.push(format!("target `{target}`: {error}")),
        }
    }
    if refusals.is_empty() {
        Ok(served)
    } else {
        Err(refusals)
    }
}

/// A scope parsed into its interface's type: what a launcher lets a client
/// of one daemon target do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonScope {
    StackCopies(StackCopiesScope),
    Fleet(FleetScope),
}

impl DaemonScope {
    /// The interface this scope belongs to.
    pub fn interface(&self) -> DaemonInterface {
        match self {
            Self::StackCopies(_) => DaemonInterface::StackCopies,
            Self::Fleet(_) => DaemonInterface::Fleet,
        }
    }

    /// The checks a launch and `peppy stack resolve` run on the scope
    /// against the launcher that gives it and the stack `selection`
    /// resolves: one refusal per problem. When the scope holds, returns the
    /// nodes that a join the scope allows can bring.
    pub fn check_launch(
        &self,
        prepared: &PreparedLauncher,
        selection: &UnitSelection,
    ) -> Result<JoinableNodes, Vec<String>> {
        match self {
            Self::StackCopies(scope) => scope.check_launch(prepared, selection),
            // A fleet scope names members of the interface alone, so every
            // launcher holds it, and a member brings no node to join.
            Self::Fleet(_) => Ok(JoinableNodes::default()),
        }
    }

    /// Narrows the published input schemas of the tool and task entries of
    /// `bundle` that daemon target `target` serves to what this scope
    /// allows, so the runtime refuses input outside the scope before a call
    /// reaches the daemon. Entries of other targets keep their schemas.
    pub fn narrow(&self, bundle: &mut ExposureBundle, target: &str) {
        match self {
            Self::StackCopies(scope) => scope.narrow(bundle, target),
            // No input of `fleet:v1` takes a value that a scope limits.
            Self::Fleet(_) => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
