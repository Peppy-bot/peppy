//! The clocks a stack change runs on: the domains its launcher declares, the
//! lifetime each one is minted for, and what the change tells the operator
//! about them.
//!
//! Resolution and minting both live in `daemon_config`, which holds the
//! launcher document's rules and the type a lifetime has to satisfy. What
//! belongs here is the part only a running daemon can do: decide which domains
//! a change introduces, carry forward the lifetimes already running, and say
//! what it resolved.

use super::feedback::publish_stdout;
use crate::services::stack::action::StackChangeContext;
use core_node_api::encoding::LaunchFeedbackStep;
use daemon_config::launcher::{
    ClockIncarnations, PeppyLauncher, Placements, ResolvedClocks, resolve_clocks,
};

/// Resolves the clock of every instance of `flat`, minting a lifetime for
/// each simulated domain the change introduces.
///
/// `known` carries the lifetimes the running stack already minted, so a join
/// follows the domains the stack is already publishing instead of replacing
/// them. A launch passes an empty map and mints every domain fresh, which is
/// what makes a replacement a new timeline even under the old names.
pub(in crate::services::stack) fn plan_clocks(
    flat: &PeppyLauncher,
    placements: &Placements,
    known: &ClockIncarnations,
) -> std::result::Result<(ResolvedClocks, ClockIncarnations), String> {
    let mut incarnations = known.clone();
    for (domain, declaration) in &flat.framework.clocks {
        if declaration.publisher().is_some() && !incarnations.contains_key(domain) {
            incarnations.insert(domain.clone(), daemon_config::launcher::mint_incarnation());
        }
    }
    let resolved = resolve_clocks(flat, placements, &incarnations).map_err(|errors| {
        daemon_config::format_bulleted(errors.iter().map(ToString::to_string).collect::<Vec<_>>())
    })?;
    Ok((resolved, incarnations))
}

/// One feedback line per simulated domain, naming the instance that supplies
/// it and the identity its consumers address.
///
/// A domain whose publisher never ticks leaves every instance bound to it
/// waiting at `clock not ready`, so the launch names the suspect while it is
/// still on screen.
pub(in crate::services::stack) async fn announce_clock_domains(
    ctx: &StackChangeContext,
    clocks: &ResolvedClocks,
) {
    for (instance, domain) in clocks.simulated() {
        if !clocks.of(instance).is_publisher() {
            continue;
        }
        publish_stdout(
            ctx,
            format!(
                "instance `{instance}` publishes clock `{}` ({domain})",
                domain.name
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }
}
