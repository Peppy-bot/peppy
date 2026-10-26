//! The launch checks of daemon scopes: every instance that has a daemon
//! target gives each one a scope that parses into its interface's type and
//! holds against the launcher, runs on the coordinator outside every copy,
//! links none of them and has at most one of each interface; no other
//! instance declares a scope.
//!
//! An instance has the daemon targets of its deployment: those of the
//! exposures of a built-in MCP deployment, and those that the manifest of a
//! node lists in `depends_on.daemon`. When a launcher is read, a scope is a
//! free value, because only the resolution of the deployments knows which
//! targets are daemon targets. The launch, each join and `peppy stack
//! resolve` run these checks once that resolution is done and before
//! anything starts. They read the flat launcher and the targets of each
//! deployment alone.
//!
//! The launch and `peppy stack resolve` also hold each scope against the
//! launcher and the stack selection ([`ScopeCheck::Launch`]), which gives the
//! joinable nodes of the launch. A join runs the rules of each instance alone
//! ([`ScopeCheck::Join`]): the launcher and the stack selection of a launch
//! do not change, so a join cannot change that verdict.

use super::compose::{CopyMembership, PreparedLauncher, UnitSelection};
use super::joinable::JoinableNodes;
use super::types::Deployment;
use crate::internal::daemon_interface::{DaemonInterface, TargetScope, served_targets};
use config::node::Manifest;
use std::collections::{BTreeMap, BTreeSet};

/// The targets of one deployment, by name: each target that the launcher's
/// `links` fill, and each daemon target with the interface this peppy serves
/// for it, which takes no link and which the launcher scopes with
/// `daemon_scopes`. For a built-in MCP deployment, the targets of its
/// exposures ([`DeploymentTargets::of_exposures`]); for a node, its slots
/// ([`DeploymentTargets::of_node`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentTargets {
    pub linked: BTreeSet<String>,
    pub daemon: BTreeMap<String, DaemonInterface>,
}

impl DeploymentTargets {
    /// The targets of a node whose manifest is `manifest`: each slot of its
    /// `depends_on`, and each daemon target, a `depends_on.daemon` entry,
    /// named by its link_id. Every daemon target must name an interface this
    /// peppy serves at the tag it names; every one that does not is reported
    /// at once.
    pub fn of_node(manifest: &Manifest) -> Result<Self, Vec<String>> {
        let Some(depends_on) = &manifest.depends_on else {
            return Ok(Self::default());
        };
        let daemon: BTreeMap<String, DaemonInterface> =
            served_targets(depends_on)?.into_iter().collect();
        let linked = depends_on
            .link_ids()
            .filter(|link_id| !daemon.contains_key(*link_id))
            .map(str::to_owned)
            .collect();
        Ok(Self { linked, daemon })
    }
}

/// One reason the daemon scopes of a launch do not hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonScopeError {
    #[error(
        "instance `{instance}` declares `daemon_scopes`, but it has no daemon target; a scope \
         belongs to an instance of a node whose manifest lists `depends_on.daemon`, or of an \
         exposure deployment whose target names `daemon: {{ name, tag }}`"
    )]
    ScopesWithoutDaemonTarget { instance: String },
    #[error(
        "`daemon_scopes.{key}` of instance `{instance}` names none of its daemon targets{}; its \
         daemon targets: {daemon_targets}",
        linked_hint(.key, *.linked_target)
    )]
    UnknownScopeKey {
        instance: String,
        key: String,
        linked_target: bool,
        daemon_targets: String,
    },
    #[error(
        "daemon target `{daemon_target}` ({interface}) of instance `{instance}` has no scope; \
         give it one in the instance's `daemon_scopes`, or with a `set_daemon_scopes` \
         adjustment on `{instance}` in the launcher, or leave it unserved with \
         `{{ vacant: \"<why>\" }}`"
    )]
    MissingScope {
        instance: String,
        daemon_target: String,
        interface: String,
    },
    #[error(
        "the scope of daemon target `{daemon_target}` ({interface}) on instance `{instance}` does \
         not parse: {reason}"
    )]
    InvalidScope {
        instance: String,
        daemon_target: String,
        interface: String,
        reason: String,
    },
    #[error(
        "the scope of daemon target `{daemon_target}` ({interface}) on instance `{instance}` \
         does not hold against the launcher: {reason}"
    )]
    ScopeRefusedByLauncher {
        instance: String,
        daemon_target: String,
        interface: String,
        reason: String,
    },
    #[error(
        "instance `{instance}` has daemon target `{daemon_target}` and declares `core_node: \
         \"{core_node}\"`; an instance with a daemon target runs on the coordinator, the \
         daemon whose stack it reads and changes, so remove `core_node`"
    )]
    PlacedOffCoordinator {
        instance: String,
        daemon_target: String,
        core_node: String,
    },
    #[error(
        "instance `{instance}` of copy `{copy}` has daemon target `{daemon_target}`; an \
         instance with a daemon target runs on the coordinator and belongs to the stack, so \
         deploy it outside the options of an axis that runs as copies"
    )]
    InCopy {
        instance: String,
        copy: String,
        daemon_target: String,
    },
    #[error(
        "`links.{daemon_target}` of instance `{instance}` names daemon target \
         `{daemon_target}`, which takes no link: the daemon that runs the instance serves it, \
         and the target takes a scope instead, in `daemon_scopes` or with `set_daemon_scopes`"
    )]
    LinkToDaemonTarget {
        instance: String,
        daemon_target: String,
    },
    #[error(
        "instance `{instance}` has daemon targets {daemon_targets} of one interface, \
         `{interface}`; a call names no daemon target, so the daemon reads each call of an \
         interface against the one scope of that interface that its sender has: give the \
         instance one daemon target of each interface"
    )]
    TargetsOfOneInterface {
        instance: String,
        interface: String,
        daemon_targets: String,
    },
}

fn linked_hint(key: &str, linked_target: bool) -> String {
    if linked_target {
        format!(" (`{key}` takes `links`, not a scope)")
    } else {
        String::new()
    }
}

/// Every reason the daemon scopes of a launch do not hold, at once.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the launcher's daemon scopes do not hold:{}",
    crate::error::format_bulleted(.0)
)]
pub struct DaemonScopeRefusals(pub Vec<DaemonScopeError>);

/// Which checks a run holds each scope to.
#[derive(Debug, Clone, Copy)]
pub enum ScopeCheck<'a> {
    /// A launch, or `peppy stack resolve`, of the stack this selection
    /// resolves: the rules of each instance, and each scope against the
    /// launcher and the stack.
    Launch(&'a UnitSelection),
    /// A join: the rules of each instance alone.
    Join,
}

/// What the checks of the daemon scopes of one run gather: every refusal,
/// and the joinable nodes of the scopes that hold.
#[derive(Default)]
struct ScopeVerdicts {
    refusals: Vec<DaemonScopeError>,
    joinable: JoinableNodes,
}

/// Checks the daemon scopes of the flat launcher whose deployments
/// `deployments` lists, each beside its targets. `prepared` is the launcher
/// the stack was composed
/// from, which a scope's own checks read (the options of the axes that run
/// as copies and the copies they compose, for `stack_copies`); `copies`
/// tells which instances belong to a copy. Every refusal is reported at
/// once.
///
/// When the scopes hold, returns the joinable nodes of the launch: the union
/// of the nodes that a join each scope allows can bring. A join runs no check
/// against the launcher, so it gets none.
pub fn check_daemon_scopes<'a>(
    prepared: &PreparedLauncher,
    deployments: impl IntoIterator<Item = (&'a Deployment, &'a DeploymentTargets)>,
    copies: &CopyMembership,
    check: ScopeCheck<'_>,
) -> Result<JoinableNodes, DaemonScopeRefusals> {
    let mut verdicts = ScopeVerdicts::default();
    for (deployment, targets) in deployments {
        for instance in &deployment.instances {
            check_instance(prepared, instance, targets, copies, check, &mut verdicts);
        }
    }
    if verdicts.refusals.is_empty() {
        Ok(verdicts.joinable)
    } else {
        Err(DaemonScopeRefusals(verdicts.refusals))
    }
}

fn check_instance(
    prepared: &PreparedLauncher,
    instance: &super::types::DeploymentInstance,
    targets: &DeploymentTargets,
    copies: &CopyMembership,
    check: ScopeCheck<'_>,
    verdicts: &mut ScopeVerdicts,
) {
    let refusals = &mut verdicts.refusals;
    let id = instance.instance_id.as_str();
    let Some(first_target) = targets.daemon.keys().next() else {
        if !instance.daemon_scopes.is_empty() {
            refusals.push(DaemonScopeError::ScopesWithoutDaemonTarget {
                instance: id.to_owned(),
            });
        }
        return;
    };

    if let Some(copy) = copies.copy_of(id) {
        refusals.push(DaemonScopeError::InCopy {
            instance: id.to_owned(),
            copy: copy.to_string(),
            daemon_target: first_target.clone(),
        });
    } else if let Some(core_node) = &instance.core_node {
        refusals.push(DaemonScopeError::PlacedOffCoordinator {
            instance: id.to_owned(),
            daemon_target: first_target.clone(),
            core_node: core_node.clone(),
        });
    }

    refusals.extend(targets_of_one_interface(id, targets));

    for daemon_target in instance
        .links
        .keys()
        .filter(|key| targets.daemon.contains_key(key.as_str()))
    {
        refusals.push(DaemonScopeError::LinkToDaemonTarget {
            instance: id.to_owned(),
            daemon_target: daemon_target.clone(),
        });
    }

    for key in instance
        .daemon_scopes
        .keys()
        .filter(|key| !targets.daemon.contains_key(key.as_str()))
    {
        refusals.push(DaemonScopeError::UnknownScopeKey {
            instance: id.to_owned(),
            key: key.clone(),
            linked_target: targets.linked.contains(key),
            daemon_targets: crate::error::format_quoted_list(targets.daemon.keys()),
        });
    }

    for (daemon_target, interface) in &targets.daemon {
        check_scope(
            prepared,
            id,
            daemon_target,
            *interface,
            instance.daemon_scopes.get(daemon_target),
            check,
            verdicts,
        );
    }
}

/// One refusal for each interface that two daemon targets of `targets` or
/// more name, on the instance `instance`.
fn targets_of_one_interface(
    instance: &str,
    targets: &DeploymentTargets,
) -> impl Iterator<Item = DaemonScopeError> {
    let mut by_interface: BTreeMap<DaemonInterface, Vec<&str>> = BTreeMap::new();
    for (daemon_target, interface) in &targets.daemon {
        by_interface
            .entry(*interface)
            .or_default()
            .push(daemon_target);
    }
    by_interface
        .into_iter()
        .filter(|(_, daemon_targets)| daemon_targets.len() > 1)
        .map(
            move |(interface, daemon_targets)| DaemonScopeError::TargetsOfOneInterface {
                instance: instance.to_owned(),
                interface: interface.label(),
                daemon_targets: crate::error::format_quoted_list(daemon_targets),
            },
        )
}

/// One daemon target's scope: present, a vacancy or parsed into its
/// interface's type, and, at a launch, held to the interface's checks
/// against the launcher, whose joinable nodes join those of the other
/// scopes.
fn check_scope(
    prepared: &PreparedLauncher,
    instance: &str,
    daemon_target: &str,
    interface: DaemonInterface,
    scope: Option<&serde_json::Value>,
    check: ScopeCheck<'_>,
    verdicts: &mut ScopeVerdicts,
) {
    let refusals = &mut verdicts.refusals;
    let Some(scope) = scope else {
        refusals.push(DaemonScopeError::MissingScope {
            instance: instance.to_owned(),
            daemon_target: daemon_target.to_owned(),
            interface: interface.label(),
        });
        return;
    };
    let parsed = match interface.parse_target_scope(scope) {
        Ok(TargetScope::Scoped(parsed)) => parsed,
        // A vacant target is served to no client, so nothing of the
        // launcher holds it, and it brings no node to join.
        Ok(TargetScope::Vacant(_)) => return,
        Err(reason) => {
            refusals.push(DaemonScopeError::InvalidScope {
                instance: instance.to_owned(),
                daemon_target: daemon_target.to_owned(),
                interface: interface.label(),
                reason,
            });
            return;
        }
    };
    let ScopeCheck::Launch(selection) = check else {
        return;
    };
    match parsed.check_launch(prepared, selection) {
        Ok(joinable) => verdicts.joinable.merge(joinable),
        Err(reasons) => refusals.extend(reasons.into_iter().map(|reason| {
            DaemonScopeError::ScopeRefusedByLauncher {
                instance: instance.to_owned(),
                daemon_target: daemon_target.to_owned(),
                interface: interface.label(),
                reason,
            }
        })),
    }
}
