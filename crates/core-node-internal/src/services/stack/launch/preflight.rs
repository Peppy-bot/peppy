//! What every stack change establishes before it touches a machine: the
//! peers it reserves and the bind sources each machine's containers need.
//! Every refusal here leaves every stack exactly as it was.

use super::PlannedDeployment;
use super::federated::{self, ReservedParticipants};
use crate::services::stack::action::StackChangeContext;
use crate::services::stack::container_mounts::container_mount_sources_by_machine;
use daemon_config::launcher::Placements;
use std::collections::HashMap;

/// A change cleared to touch its machines, holding their reservations.
pub(in crate::services::stack) struct ChangePlan {
    pub reserved: ReservedParticipants,
    /// The host paths each touched machine's containers bind.
    pub mounts: HashMap<String, Vec<String>>,
}

/// Clears a change over `touched`, the part of the plan it starts, stops or
/// depends on: those machines are reserved and their container bind sources
/// resolved, with every stack left exactly as it was.
pub(in crate::services::stack) async fn preflight_change(
    ctx: &StackChangeContext,
    launch_id: &str,
    touched: &[PlannedDeployment],
    placements: &Placements,
) -> Result<ChangePlan, String> {
    let reserved = federated::preflight(ctx, launch_id, touched, placements).await?;
    let mounts = container_mount_sources_by_machine(touched, placements)?;
    Ok(ChangePlan { reserved, mounts })
}
