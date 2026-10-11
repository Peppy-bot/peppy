//! The daemon bridge: the provider of the tools of the daemon targets.
//!
//! A daemon target names an interface of the peppy daemon, which no node
//! implements. To the runtime, its tools are services and actions as the
//! tools of a contract target are, and this bridge is their provider: it
//! calls the daemon that started the server, which is the coordinator,
//! through the goal path that the CLI uses (the `stack-goal` crate).
//!
//! The server holds one bridge per interface it serves, and each daemon
//! target has its own scope, which the launch gave the instance. The server
//! parses the scopes at start ([`DaemonScopes::parse`]), narrows the published
//! schemas of each target with its scope ([`DaemonScopes::narrow`]), and then
//! registers one handler per entry ([`DaemonBridges::tool`] and
//! [`DaemonBridges::task`]). A server whose scope is missing or does not
//! parse does not start.
//!
//! `stack_copies:v1` is the one interface: [`stack_copies`] lists, adds and
//! removes the copies of the running launch.

mod stack_copies;

#[cfg(test)]
mod tests;

use crate::serve::ServeError;
use daemon_config::daemon_interface::{DaemonInterface, DaemonScope, StackCopiesMember};
use peppy_mcp_catalog::{
    BundleDaemonTarget, DaemonInterfaceRef, ExposureBundle, GoalBound, TaskEntry, ToolEntry,
};
use peppy_mcp_runtime::{ActionContext, TaskHandler, ToolCall, ToolHandler};
use peppylib::messaging::MessengerHandle;
use peppylib::runtime::NodeRunner;
use serde_json::Value;
use stack_goal::DaemonRoute;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use stack_copies::{ScopedStackCopies, StackCopies};

/// The daemon that started the server, as the bridge reaches it: the
/// session and the identity of the server's node, and the daemon's core
/// node. It is owned, so a task that follows a goal after its call ended
/// holds it.
#[derive(Clone)]
pub(crate) struct OwnDaemon {
    messenger: MessengerHandle,
    /// The core node the server is bound to, which is its daemon's.
    core_node: String,
    /// The server's instance id, which every request is sent as.
    instance_id: String,
}

impl OwnDaemon {
    /// The daemon `node_runner` is bound to.
    pub(crate) fn of(node_runner: &NodeRunner) -> Self {
        let route = DaemonRoute::own_daemon(node_runner);
        Self::new(
            route.messenger.clone(),
            route.daemon_core_node,
            route.caller_instance_id,
        )
    }

    /// The daemon of core node `core_node`, reached on `messenger` as the
    /// node instance `instance_id` bound to it.
    pub(crate) fn new(messenger: MessengerHandle, core_node: &str, instance_id: &str) -> Self {
        Self {
            messenger,
            core_node: core_node.to_owned(),
            instance_id: instance_id.to_owned(),
        }
    }

    /// The route of a goal or a request to the daemon.
    fn route(&self) -> DaemonRoute<'_> {
        DaemonRoute {
            messenger: &self.messenger,
            caller_core_node: &self.core_node,
            caller_instance_id: &self.instance_id,
            daemon_core_node: &self.core_node,
        }
    }
}

/// The interface a daemon target names, which planning already found this
/// peppy serves.
fn interface_of(daemon: &BundleDaemonTarget) -> Result<DaemonInterface, ServeError> {
    let unserved = |reason: String| ServeError::UnservedDaemonInterface {
        target: daemon.target.clone(),
        reason,
    };
    let name =
        config::runtime::Name::new(&daemon.name).map_err(|error| unserved(error.to_string()))?;
    DaemonInterface::served(&DaemonInterfaceRef {
        name,
        tag: daemon.tag.clone(),
    })
    .map_err(|error| unserved(error.to_string()))
}

/// The daemon function behind a tool of a daemon target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonService {
    /// `stack_copies` `list`: the scope's options and the copies of them.
    ListCopies,
}

/// The daemon function behind a task of a daemon target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonAction {
    /// `stack_copies` `join`: adds a copy of a scoped option.
    JoinCopy,
    /// `stack_copies` `remove`: removes a copy of a scoped option.
    RemoveCopy,
}

/// A tool entry of a daemon target, ready to bind to its target's bridge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DaemonTool {
    pub name: String,
    pub target: String,
    pub service: DaemonService,
    /// The tool's deadline, which bounds the bridge's request to the daemon.
    pub deadline: Duration,
}

impl DaemonTool {
    /// The tool `entry` of daemon target `daemon`.
    pub(crate) fn new(entry: &ToolEntry, daemon: &BundleDaemonTarget) -> Result<Self, ServeError> {
        let service = match (
            interface_of(daemon)?,
            StackCopiesMember::named(&entry.member),
        ) {
            (DaemonInterface::StackCopies, Some(StackCopiesMember::List)) => {
                DaemonService::ListCopies
            }
            (interface, _) => {
                return Err(unbridged_member(
                    &entry.name,
                    &entry.member,
                    interface,
                    "a service",
                ));
            }
        };
        Ok(Self {
            name: entry.name.clone(),
            target: entry.target.clone(),
            service,
            deadline: Duration::from_millis(entry.deadline_ms.get()),
        })
    }
}

/// A task entry of a daemon target, ready to bind to its target's bridge.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DaemonTask {
    pub name: String,
    pub target: String,
    pub action: DaemonAction,
    /// The progress window of the document: the most silence the bridge
    /// waits through. `None` under a whole-goal deadline, which the runtime
    /// holds the call to.
    pub window: Option<Duration>,
}

impl DaemonTask {
    /// The task `entry` of daemon target `daemon`.
    pub(crate) fn new(entry: &TaskEntry, daemon: &BundleDaemonTarget) -> Result<Self, ServeError> {
        let action = match (
            interface_of(daemon)?,
            StackCopiesMember::named(&entry.member),
        ) {
            (DaemonInterface::StackCopies, Some(StackCopiesMember::Join)) => DaemonAction::JoinCopy,
            (DaemonInterface::StackCopies, Some(StackCopiesMember::Remove)) => {
                DaemonAction::RemoveCopy
            }
            (interface, _) => {
                return Err(unbridged_member(
                    &entry.name,
                    &entry.member,
                    interface,
                    "an action",
                ));
            }
        };
        let window = match entry.bound {
            GoalBound::Progress { window_ms } => Some(Duration::from_millis(window_ms.get())),
            GoalBound::WholeGoal { .. } => None,
        };
        Ok(Self {
            name: entry.name.clone(),
            target: entry.target.clone(),
            action,
            window,
        })
    }
}

/// The refusal of an entry whose member the bridge does not serve as an
/// entry of `kind`. Validation binds every entry to a member of its kind, so
/// a served exposure never gets here.
fn unbridged_member(
    tool: &str,
    member: &str,
    interface: DaemonInterface,
    kind: &'static str,
) -> ServeError {
    ServeError::UnbridgedDaemonMember {
        tool: tool.to_owned(),
        member: member.to_owned(),
        interface: interface.label(),
        kind,
    }
}

/// One reason the server cannot serve a daemon target with the scopes the
/// launch gave its instance.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum ScopeProblem {
    #[error(
        "daemon target `{target}` ({interface}) has no scope; give it one in the `daemon_scopes` \
         of instance `{instance}`, or with a `set_daemon_scopes` adjustment on `{instance}` in \
         the launcher"
    )]
    Missing {
        target: String,
        interface: String,
        instance: String,
    },
    #[error(
        "the scope of daemon target `{target}` ({interface}) does not parse: {reason}; correct it \
         in the `daemon_scopes` of instance `{instance}`, or in the `set_daemon_scopes` \
         adjustment on `{instance}` in the launcher"
    )]
    Invalid {
        target: String,
        interface: String,
        instance: String,
        reason: String,
    },
    #[error(
        "`daemon_scopes.{key}` of instance `{instance}` names no daemon target of this server; \
         its daemon targets: {targets}"
    )]
    UnknownKey {
        key: String,
        instance: String,
        targets: String,
    },
}

/// The scope of each daemon target of the server, parsed into the type of
/// its interface.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DaemonScopes {
    by_target: BTreeMap<String, DaemonScope>,
    /// The server's instance, which a refusal names.
    instance: String,
}

impl DaemonScopes {
    /// Parses `scopes`, the scopes the launch gave instance `instance`, for
    /// `targets`, the daemon targets of the server with the interface each
    /// names. Every daemon target takes a scope that parses into its
    /// interface's type, and every scope names a daemon target; each problem
    /// is reported at once. A launch and a join check the scopes before they
    /// start the server, so the server finds a problem only when it starts
    /// some other way, for example with `peppy node run`.
    pub(crate) fn parse(
        instance: &str,
        targets: &BTreeMap<String, DaemonInterface>,
        scopes: &BTreeMap<String, Value>,
    ) -> Result<Self, ServeError> {
        let mut problems = Vec::new();
        let mut by_target = BTreeMap::new();
        for (target, interface) in targets {
            let Some(value) = scopes.get(target) else {
                problems.push(ScopeProblem::Missing {
                    target: target.clone(),
                    interface: interface.label(),
                    instance: instance.to_owned(),
                });
                continue;
            };
            match interface.parse_scope(value) {
                Ok(scope) => {
                    by_target.insert(target.clone(), scope);
                }
                Err(reason) => problems.push(ScopeProblem::Invalid {
                    target: target.clone(),
                    interface: interface.label(),
                    instance: instance.to_owned(),
                    reason,
                }),
            }
        }
        for key in scopes.keys().filter(|key| !targets.contains_key(*key)) {
            problems.push(ScopeProblem::UnknownKey {
                key: key.clone(),
                instance: instance.to_owned(),
                targets: daemon_config::format_quoted_list(targets.keys()),
            });
        }
        if !problems.is_empty() {
            return Err(ServeError::DaemonScopes {
                problems: daemon_config::format_bulleted(problems),
            });
        }
        Ok(Self {
            by_target,
            instance: instance.to_owned(),
        })
    }

    /// Narrows the published input schemas of every daemon target of
    /// `bundle` to its scope, so the runtime refuses input outside the scope
    /// before a call reaches the bridge. The entries of other targets keep
    /// their schemas.
    pub(crate) fn narrow(&self, bundle: &mut ExposureBundle) {
        for (target, scope) in &self.by_target {
            scope.narrow(bundle, target);
        }
    }

    /// The scope of daemon target `target`, which names `interface`.
    /// [`Self::parse`] gave a scope to every daemon target of the server.
    fn of(&self, target: &str, interface: DaemonInterface) -> Result<&DaemonScope, ServeError> {
        self.by_target
            .get(target)
            .ok_or_else(|| ServeError::DaemonScopes {
                problems: daemon_config::format_bulleted([ScopeProblem::Missing {
                    target: target.to_owned(),
                    interface: interface.label(),
                    instance: self.instance.clone(),
                }]),
            })
    }
}

/// The daemon bridges of one server: one per interface it serves, and the
/// scope of each daemon target, which the handlers of the target's entries
/// carry.
pub(crate) struct DaemonBridges {
    stack_copies: Arc<StackCopies>,
    scopes: DaemonScopes,
}

impl DaemonBridges {
    /// The bridges to `daemon`, the daemon that started the server, under
    /// `scopes`.
    pub(crate) fn new(daemon: OwnDaemon, scopes: DaemonScopes) -> Self {
        Self {
            stack_copies: Arc::new(StackCopies::new(daemon)),
            scopes,
        }
    }

    /// Narrows the schemas of the daemon targets of `bundle` to their scopes
    /// (see [`DaemonScopes::narrow`]).
    pub(crate) fn narrow(&self, bundle: &mut ExposureBundle) {
        self.scopes.narrow(bundle);
    }

    /// The handler of tool `tool`: its target's bridge under its target's
    /// scope.
    pub(crate) fn tool(&self, tool: &DaemonTool) -> Result<impl ToolHandler + use<>, ServeError> {
        match tool.service {
            DaemonService::ListCopies => {
                let bridge = self.stack_copies_of(&tool.target)?;
                let deadline = tool.deadline;
                Ok(move |_call: ToolCall| {
                    let bridge = bridge.clone();
                    async move { bridge.list(deadline).await }
                })
            }
        }
    }

    /// The handler of task `task`: its target's bridge under its target's
    /// scope.
    pub(crate) fn task(&self, task: &DaemonTask) -> Result<impl TaskHandler + use<>, ServeError> {
        let bridge = self.stack_copies_of(&task.target)?;
        let action = task.action;
        let window = task.window;
        Ok(move |call: ToolCall, context: ActionContext| {
            let bridge = bridge.clone();
            async move {
                match action {
                    DaemonAction::JoinCopy => bridge.join(&call.input, &context, window).await,
                    DaemonAction::RemoveCopy => bridge.remove(&call.input, &context, window).await,
                }
            }
        })
    }

    /// The `stack_copies` bridge under the scope of daemon target `target`.
    fn stack_copies_of(&self, target: &str) -> Result<ScopedStackCopies, ServeError> {
        let DaemonScope::StackCopies(scope) =
            self.scopes.of(target, DaemonInterface::StackCopies)?;
        Ok(self.stack_copies.scoped(scope.clone()))
    }
}
