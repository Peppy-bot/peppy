//! The sender of a call of a daemon interface, as the scopes of the launch
//! read it.

use crate::services::federation::SliceOwnership;
use core_node_api::names::COMMAND_LINE_INSTANCE_ID;
use daemon_config::daemon_interface::{DaemonInterface, DaemonScope, TargetScope};

/// Who sends a call.
#[derive(Debug, Clone)]
pub(crate) enum Caller {
    /// The command line: it acts for the person or program that runs it,
    /// and has no scope.
    CommandLine,
    /// An instance of the launch that this daemon coordinates, with the
    /// scope of each daemon target it serves, and whether it is a built-in
    /// MCP server.
    Instance {
        mcp_server: bool,
        scopes: Vec<DaemonScope>,
    },
    /// Any other sender: no scope.
    Other,
}

impl Caller {
    /// The caller that sends as `instance_id`.
    pub(crate) fn of(ownership: &SliceOwnership, instance_id: &str) -> Self {
        if instance_id == COMMAND_LINE_INSTANCE_ID {
            return Self::CommandLine;
        }
        let active = ownership.active.lock();
        let Some(targets) = active
            .as_ref()
            .and_then(|active| active.daemon_targets_of(instance_id))
        else {
            return Self::Other;
        };
        let scopes = targets
            .targets
            .iter()
            .filter_map(
                |(interface, value)| match interface.parse_target_scope(value) {
                    Ok(TargetScope::Scoped(scope)) => Some(scope),
                    Ok(TargetScope::Vacant(_)) | Err(_) => None,
                },
            )
            .collect();
        Self::Instance {
            mcp_server: targets.mcp_server,
            scopes,
        }
    }

    /// The scope of the caller's daemon target of `interface`, if it has
    /// one.
    pub(crate) fn scope(&self, interface: DaemonInterface) -> Option<&DaemonScope> {
        match self {
            Self::Instance { scopes, .. } => {
                scopes.iter().find(|scope| scope.interface() == interface)
            }
            Self::CommandLine | Self::Other => None,
        }
    }

    /// Whether the caller is a built-in MCP server of peppy.
    pub(crate) fn is_mcp_server(&self) -> bool {
        matches!(
            self,
            Self::Instance {
                mcp_server: true,
                ..
            }
        )
    }
}
