//! Telling other daemons what happened to an instance this one owns.
//!
//! # Why this exists at all
//!
//! Every cross-daemon relationship has an authoritative side: the daemon that
//! owns the instance. It sees the process reach Running, and it sees it die.
//! The other side sees nothing, because a lifecycle transition is a local event
//! on the machine it happens on. Without this module the receiving half of
//! `relationship_notify` would have nothing to receive: an observer would never
//! learn its remote source came up, and a pair would never dissolve when its
//! remote peer died.
//!
//! # Who gets told
//!
//! Two sources, because the two relationships know different things:
//!
//! * **Pairing** is symmetric and explicit. A pair names both endpoints and the
//!   core node each runs on, so this daemon reads the recipients straight out
//!   of its own registry. That also covers pairs formed outside a launch, by a
//!   `peppy node run --pair` naming a peer on another machine.
//!
//! * **Observation** is deliberately one-way and invisible to the source: an
//!   observer claims no slot, holds no peer, and the source is not told it
//!   exists. That is the property that makes an observer unable to perturb
//!   control, and it is exactly why this daemon cannot work out who to tell.
//!   Only the planner sees the whole graph, so the planner names them on the
//!   `NodeRunGoal` and [`WatcherRegistry`] records them.
//!
//! # Best-effort on purpose
//!
//! A notification reports what has ALREADY happened on the authoritative side,
//! so a duplicate changes nothing and a lost one leaves the receiver stale
//! rather than in disagreement. Failures are logged, never propagated: a launch
//! must not fail because a peer was slow to acknowledge something that is
//! already true. It is also why a node whose correctness depends on freshness
//! owns a staleness watchdog rather than trusting the framework to notice a
//! partition, which no notification can survive.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{RelationshipEvent, RelationshipNotification};
use parking_lot::Mutex;
use peppylib::MessengerHandle;
use peppylib::core_node::transport::poll;

use super::PairingCoordinator;

/// Bound on one notification. Short: the receiver only records what already
/// happened, and this runs on lifecycle paths that must not stall behind a
/// slow peer.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Which daemons the planner said are observing each local instance.
///
/// Pure, and separate from the messenger for the same reason `SliceOwnership`
/// is: the interesting rules here (replace rather than merge, never notify
/// yourself) are decidable without any I/O, so they are tested without any.
#[derive(Debug, Default)]
pub(crate) struct WatcherRegistry {
    by_instance: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

impl WatcherRegistry {
    /// Records the daemons whose observers tap `instance_id`.
    ///
    /// Replaces any previous set rather than merging: a re-run of the same
    /// instance id is a NEW instance with a new plan, and carrying the old
    /// plan's watchers forward would keep notifying a daemon whose observer is
    /// long gone.
    pub(crate) fn set(&self, instance_id: &str, core_nodes: &[String], self_core_node: &str) {
        let recipients: BTreeSet<String> = core_nodes
            .iter()
            .filter(|core_node| core_node.as_str() != self_core_node)
            .cloned()
            .collect();
        let mut by_instance = self.by_instance.lock();
        if recipients.is_empty() {
            by_instance.remove(instance_id);
        } else {
            by_instance.insert(instance_id.to_owned(), recipients);
        }
    }

    pub(crate) fn forget(&self, instance_id: &str) {
        self.by_instance.lock().remove(instance_id);
    }

    /// The union of the planner-named watchers and the pair peers this daemon
    /// derived itself, minus this daemon. Telling yourself something you
    /// already know is noise, and on a single-machine launch it would be every
    /// notification.
    pub(crate) fn recipients(
        &self,
        instance_id: &str,
        pair_peers: BTreeSet<String>,
        self_core_node: &str,
    ) -> BTreeSet<String> {
        let mut recipients = self
            .by_instance
            .lock()
            .get(instance_id)
            .cloned()
            .unwrap_or_default();
        recipients.extend(pair_peers);
        recipients.remove(self_core_node);
        recipients
    }
}

/// Announces local instance lifecycle transitions to the daemons that hold a
/// relationship with them.
pub(crate) struct RelationshipNotifier {
    messenger: MessengerHandle,
    core_node_name: String,
    caller_instance_id: String,
    pairing: Arc<PairingCoordinator>,
    watchers: WatcherRegistry,
}

impl RelationshipNotifier {
    pub(crate) fn new(
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
        pairing: Arc<PairingCoordinator>,
    ) -> Self {
        Self {
            messenger,
            core_node_name: core_node_name.into(),
            caller_instance_id: caller_instance_id.into(),
            pairing,
            watchers: WatcherRegistry::default(),
        }
    }

    /// Records the daemons whose observers tap `instance_id`, as named by the
    /// planner on that instance's `NodeRunGoal`.
    pub(crate) fn set_watchers(&self, instance_id: &str, core_nodes: &[String]) {
        self.watchers
            .set(instance_id, core_nodes, &self.core_node_name);
    }

    /// Reports that `instance_id` reached Running under a fresh incarnation.
    ///
    /// This is what makes a remote observer drop and redeclare its
    /// subscription across a source restart, exactly as a local one does.
    pub(crate) async fn announce_running(&self, instance_id: &str) {
        self.announce(instance_id, RelationshipEvent::ReachedRunning)
            .await;
    }

    /// Reports that `instance_id` stopped or died, and forgets its watchers.
    ///
    /// Dissolution stays authoritative on this daemon; the notification only
    /// propagates what already happened here.
    pub(crate) async fn announce_stopped(&self, instance_id: &str) {
        self.announce(instance_id, RelationshipEvent::Stopped).await;
        self.watchers.forget(instance_id);
    }

    async fn announce(&self, instance_id: &str, event: RelationshipEvent) {
        let recipients = self.watchers.recipients(
            instance_id,
            self.pairing.remote_peer_core_nodes(instance_id),
            &self.core_node_name,
        );
        if recipients.is_empty() {
            return;
        }

        let notification = RelationshipNotification::new(instance_id, &self.core_node_name, event);
        futures::future::join_all(recipients.iter().map(|core_node| {
            let notification = &notification;
            async move {
                let outcome = poll(
                    notification,
                    &self.messenger,
                    &self.core_node_name,
                    &self.caller_instance_id,
                    core_node,
                    NOTIFY_TIMEOUT,
                )
                .await;
                if let Err(e) = outcome {
                    tracing::warn!(
                        "could not tell `{core_node}` that `{instance_id}` {} ({e}); that \
                         daemon's view of this instance is stale until its next update",
                        match event {
                            RelationshipEvent::ReachedRunning => "reached Running",
                            RelationshipEvent::Stopped => "stopped",
                        }
                    );
                }
            }
        }))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(core_nodes: &[&str]) -> BTreeSet<String> {
        core_nodes.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_instance_nobody_watches_or_pairs_with_has_no_recipients() {
        let registry = WatcherRegistry::default();
        assert!(
            registry
                .recipients("wrist_cam_inst", BTreeSet::new(), "cn-robot")
                .is_empty()
        );
    }

    #[test]
    fn the_planner_named_watchers_become_recipients() {
        let registry = WatcherRegistry::default();
        registry.set("reflex_inst", &["cn-atlas".to_owned()], "cn-robot");
        assert_eq!(
            registry.recipients("reflex_inst", BTreeSet::new(), "cn-robot"),
            peers(&["cn-atlas"])
        );
    }

    /// The two sources are a union, not alternatives: an instance can be both
    /// paired across a boundary and observed from a third machine.
    #[test]
    fn pair_peers_and_watchers_are_both_notified() {
        let registry = WatcherRegistry::default();
        registry.set("reflex_inst", &["cn-atlas".to_owned()], "cn-robot");
        assert_eq!(
            registry.recipients("reflex_inst", peers(&["cn-edge"]), "cn-robot"),
            peers(&["cn-atlas", "cn-edge"])
        );
    }

    /// A daemon telling itself something it already knows is noise, and on a
    /// single-machine launch it would be every notification.
    #[test]
    fn this_daemon_never_notifies_itself() {
        let registry = WatcherRegistry::default();
        registry.set("reflex_inst", &["cn-robot".to_owned()], "cn-robot");
        assert!(
            registry
                .recipients("reflex_inst", peers(&["cn-robot"]), "cn-robot")
                .is_empty()
        );
    }

    /// A re-run of the same instance id is a NEW instance with a new plan.
    /// Merging would keep notifying a daemon whose observer is long gone.
    #[test]
    fn re_running_an_instance_replaces_its_watchers_rather_than_adding_to_them() {
        let registry = WatcherRegistry::default();
        registry.set("reflex_inst", &["cn-atlas".to_owned()], "cn-robot");
        registry.set("reflex_inst", &["cn-edge".to_owned()], "cn-robot");
        assert_eq!(
            registry.recipients("reflex_inst", BTreeSet::new(), "cn-robot"),
            peers(&["cn-edge"])
        );

        registry.set("reflex_inst", &[], "cn-robot");
        assert!(
            registry
                .recipients("reflex_inst", BTreeSet::new(), "cn-robot")
                .is_empty()
        );
    }

    /// A stopped instance's watchers go with it: the next instance to take that
    /// id gets its own plan, and until then there is nothing to report.
    #[test]
    fn forgetting_an_instance_drops_its_watchers() {
        let registry = WatcherRegistry::default();
        registry.set("reflex_inst", &["cn-atlas".to_owned()], "cn-robot");
        registry.forget("reflex_inst");
        assert!(
            registry
                .recipients("reflex_inst", BTreeSet::new(), "cn-robot")
                .is_empty()
        );
    }

    /// Pair peers still reach a forgotten instance, because they come from the
    /// pairing registry rather than from here.
    #[test]
    fn pair_peers_do_not_depend_on_the_watcher_registry() {
        let registry = WatcherRegistry::default();
        assert_eq!(
            registry.recipients("reflex_inst", peers(&["cn-atlas"]), "cn-robot"),
            peers(&["cn-atlas"])
        );
    }
}
