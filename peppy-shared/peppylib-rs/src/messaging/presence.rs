use std::time::Duration;

use pmi::{Segment, SenderTargetError};

use super::{CoreNodePresence, LivelinessToken, LivelinessWatch, MessengerHandle};
use crate::error::Result;

/// Stateless facade for advertising, watching, and enumerating live core
/// nodes in the current messenger session's namespace.
pub struct CoreNodePresenceMessenger;

impl CoreNodePresenceMessenger {
    /// Recommended [`Self::list_live`] bound: liveliness state is answered
    /// from the router's current view, normally immediately, so the timeout
    /// only protects callers from a stalled transport. Callers pass their own
    /// bound when they know better (e.g. tests on an in-process mock).
    pub const LIST_TIMEOUT: Duration = Duration::from_secs(2);

    /// Advertise one daemon generation until the returned token or its
    /// messenger session is dropped.
    pub async fn declare(
        messenger: &MessengerHandle,
        core_node: &str,
        instance_id: &str,
    ) -> Result<LivelinessToken> {
        let core_node = validated_segment(core_node)?;
        let instance_id = validated_segment(instance_id)?;
        messenger
            .declare_core_node_presence(&core_node, &instance_id)
            .await
    }

    /// Watch one core-node name, replaying current claims before live
    /// transitions.
    pub async fn watch(
        messenger: &MessengerHandle,
        core_node: &str,
    ) -> Result<LivelinessWatch<CoreNodePresence>> {
        let core_node = validated_segment(core_node)?;
        messenger.watch_core_node_presence(Some(&core_node)).await
    }

    /// List all live daemon tokens, optionally restricted to one core-node
    /// name. Duplicate names remain distinct by instance id so callers can
    /// surface active collisions; duplicate transport replies for the same
    /// `(core_node, instance_id)` token are collapsed by the messaging layer.
    pub async fn list_live(
        messenger: &MessengerHandle,
        core_node: Option<&str>,
        timeout: Duration,
    ) -> Result<Vec<CoreNodePresence>> {
        let core_node = core_node.map(validated_segment).transpose()?;
        messenger
            .list_core_node_presence(core_node.as_ref(), timeout)
            .await
    }
}

fn validated_segment(value: &str) -> Result<Segment> {
    Segment::try_from(value)
        .map_err(SenderTargetError::from)
        .map_err(Into::into)
}
