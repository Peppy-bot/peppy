//! The daemon's authority for observer-slot delivery: it holds the observer
//! registry and pushes each observer slot its whole member set over the
//! `observation_update` service, following each member source's lifecycle.
//!
//! Observation is not a registry commit like pairing, so there is nothing to
//! reserve or revert: an observer passively taps a producer's role topics
//! without joining the pairing and without claiming any slot. The coordinator
//! only tracks who observes whom (so a source coming up can notify its
//! observers) and a per-source incarnation generation (so an observer drops and
//! redeclares one member's wire subscription when that member's source
//! restarts). Delivery is best-effort: a push that fails is logged, never fatal,
//! because an observer that misses an update simply stays on its last state
//! until the next lifecycle notify.
//!
//! Delivery is per SLOT, never per member: a slot's update carries its complete
//! ordered member set, so the node replaces the slot wholesale and the plan's
//! order is the order it holds. One member's transition therefore re-sends its
//! slot's whole set, each member stamped with its own current generation and
//! liveness.
//!
//! Two counters ride each update, exactly as the wire type documents:
//! `sequence` (strictly increasing, seeded from unix-millis at daemon start)
//! rejects stale re-deliveries; a member's `source_generation` advances only
//! when that source's incarnation changes (it reaches Running again) and is the
//! sole discriminator between old-source and new-source messages on the wire.

use core_node_api::encoding::ObservationTargets;
use daemon_config::launcher::PlannedObservation;
use futures::future::join_all;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::encoding::observation_update::ObservationUpdateRequest;
use peppylib::messaging::{
    OBSERVATION_UPDATE_SERVICE, ObservedMemberState, ObservedSource, ProducerRef,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use super::common::{SlotUpdateClient, SlotUpdateTarget};

/// How long a single `observation_update` delivery may take before it is
/// treated as failed. The service is pre-setup (registered before the node's
/// ready signal), so a healthy observer answers promptly.
const OBSERVATION_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

/// A source instance's full address. Two daemons can host same-named
/// instances, so the pair is the identity: keying the registry on the instance
/// id alone would let a local `wrist_cam_inst` and a remote one share an
/// incarnation counter and cross-deliver each other's pins.
///
/// Deliberately coarser than the [`ObservedSource`] it is derived from, which
/// also carries the producer-side link_id: an incarnation counter belongs to
/// the source instance, so an instance observed through two of its own pairing
/// slots must advance one counter, not two.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceKey {
    core_node: String,
    instance_id: String,
}

impl SourceKey {
    fn new(core_node: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
        }
    }

    fn from_producer(producer: &ProducerRef) -> Self {
        Self::new(&producer.core_node, &producer.instance_id)
    }
}

/// One member of one observer slot, as the daemon holds it: a slot with N
/// members holds N records, in plan order.
///
/// The source address is read from `pin.producer` rather than duplicated
/// alongside it: the pin is what goes on the wire, so deriving the registry key
/// from it means the two can never disagree about which instance is observed.
#[derive(Debug, Clone)]
struct ObserverRecord {
    observer_link_id: String,
    pin: ObservedSource,
}

impl ObserverRecord {
    fn source(&self) -> SourceKey {
        SourceKey::from_producer(&self.pin.producer)
    }
}

/// One observer slot: the instance that declares it and its own link_id. The
/// unit of delivery, since a slot's update always carries its whole member set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SlotKey {
    observer_instance_id: String,
    observer_link_id: String,
}

/// The observer registry. Observer records are the source of truth; source
/// fan-out is derived from them in one pass when a lifecycle event arrives.
#[derive(Default)]
struct Registry {
    /// observer instance id → its observer-slot records, in plan order (one per
    /// member, so a multi-member slot appears several times). Observers are
    /// always local: delivery to an observer node stays on the daemon that owns
    /// it, even when the source is remote.
    by_observer: BTreeMap<String, Vec<ObserverRecord>>,
    /// source → its current incarnation generation. Advances only on the
    /// source reaching Running; never decreases (kept across the source's own
    /// down/up so a restart is a strictly newer generation).
    ///
    /// A remote source's transitions arrive as notifications from its own
    /// daemon and feed exactly this counter, which is what makes an observer
    /// drop and redeclare across a remote restart the same way it does across
    /// a local one.
    source_generation: BTreeMap<SourceKey, u64>,
    /// Sources on OTHER daemons currently reported up.
    ///
    /// Tracked separately from [`Self::source_generation`] because the two
    /// answer different questions: the generation is an incarnation counter
    /// that is deliberately RETAINED across a source's own down/up (so a
    /// restart is strictly newer), while this is the up/down state itself.
    /// Reading liveness off the retained generation reported a remote source
    /// live for as long as anything still observed it, which is precisely
    /// after it went down. A local source needs no entry here: the node stack
    /// is the authority for instances this daemon runs.
    remote_live: BTreeSet<SourceKey>,
}

/// One slot's pending `observation_update`: its complete ordered member set,
/// each member stamped with its own generation and liveness at assembly time.
#[derive(Debug)]
struct Delivery {
    slot: SlotKey,
    members: Vec<ObservedMemberState>,
}

/// The event's own verdict about one source, which supersedes the generic
/// liveness lookup while assembling that event's deliveries.
///
/// A lifecycle hook knows something the authorities do not yet agree on: the
/// instance that just reached Running may not be in the stack's live set at the
/// instant this runs, and the one going down may still be in it. Stamping the
/// event's own source from the event itself is what keeps "came up" and "went
/// down" from ever disagreeing with the transition that triggered them.
struct LivenessVerdict<'a> {
    source: &'a SourceKey,
    live: bool,
}

pub struct ObservationCoordinator {
    updates: SlotUpdateClient,
    /// Serializes lifecycle notifications so a source's generation bump and its
    /// fan-out delivery cannot interleave with a concurrent notify.
    op_lock: tokio::sync::Mutex<()>,
    registry: std::sync::Mutex<Registry>,
}

impl ObservationCoordinator {
    pub fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            updates: SlotUpdateClient::new(
                node_stack,
                messenger,
                core_node_name,
                caller_instance_id,
            ),
            op_lock: tokio::sync::Mutex::new(()),
            registry: std::sync::Mutex::new(Registry::default()),
        }
    }

    /// Drops the entire observer registry. The observation twin of
    /// `node_stack.reset()` clearing the pairing registry: `stack reset` tears
    /// the whole stack down at once, so without this a re-run of the same
    /// instance ids inherits stale observations and a source coming back up
    /// would deliver a pin the user never linked this time. `stack launch` does
    /// not need it because [`register_planned`] already replaces the registry.
    pub fn clear(&self) {
        *self.registry.lock().unwrap() = Registry::default();
    }

    /// Records every planned observation so later lifecycle notifications can
    /// find them. Called once by the launcher before any instance starts, so a
    /// source that comes up first still finds its waiting observers. A launch
    /// replaces the whole stack, so this replaces the registry too (the
    /// previous stack's observers are gone); passing an empty slice clears it.
    pub fn register_planned(&self, planned: &[PlannedObservation]) {
        let mut registry = self.registry.lock().unwrap();
        *registry = Registry::default();
        for obs in planned {
            Self::insert_record(
                &mut registry,
                &obs.observer_instance_id,
                ObserverRecord {
                    observer_link_id: obs.observer_link_id.clone(),
                    pin: ObservedSource {
                        producer: obs.source.clone(),
                        source_link_id: obs.source_link_id.clone(),
                    },
                },
            );
        }
    }

    /// Registers ONE `node run` instance's observer slots into the live
    /// registry without disturbing the rest of it. Unlike [`register_planned`]
    /// (which replaces the whole stack at launch), a `node run` adds a single
    /// instance to an already-running stack, so any prior records for this same
    /// observer id (a re-run) are cleared and its new ones merged in.
    ///
    /// The source's core node comes from the request rather than being assumed
    /// to be this daemon's: every message a daemon acts on is self-describing
    /// about placement, so a remote source registers here exactly like a local
    /// one and differs only in where its lifecycle transitions come from.
    ///
    /// Register BEFORE the instance reaches Running: the daemon's
    /// `on_instance_running` hook then finds these records and delivers each
    /// source pin whose source is already live, matching the launcher path.
    pub fn register_instance(
        &self,
        observer_instance_id: &str,
        observations: &BTreeMap<String, ObservationTargets>,
    ) {
        let mut registry = self.registry.lock().unwrap();
        // A re-run replaces its records rather than accumulating them.
        registry.by_observer.remove(observer_instance_id);
        for (observer_link_id, targets) in observations {
            for target in targets {
                Self::insert_record(
                    &mut registry,
                    observer_instance_id,
                    ObserverRecord {
                        observer_link_id: observer_link_id.clone(),
                        pin: ObservedSource {
                            producer: target.source.clone(),
                            source_link_id: target.source_link_id.clone(),
                        },
                    },
                );
            }
        }
    }

    /// Inserts one resolved observer record. Shared by [`register_planned`]
    /// and [`register_instance`].
    fn insert_record(registry: &mut Registry, observer_instance_id: &str, record: ObserverRecord) {
        registry
            .by_observer
            .entry(observer_instance_id.to_string())
            .or_default()
            .push(record);
    }

    /// An instance reached Running. If it is a source, its incarnation
    /// generation advances and every slot observing it is re-delivered whole.
    /// If it is itself an observer, every slot it declares is delivered, so it
    /// learns its member set even while some members are still down. Both can
    /// hold for one instance.
    pub async fn on_instance_running(&self, instance_id: &str) {
        let source = SourceKey::new(self.updates.core_node_name(), instance_id);
        self.source_reached_running(&source, Some(instance_id))
            .await;
    }

    /// A source on ANOTHER daemon reached Running, as reported by that daemon.
    ///
    /// An observing daemon cannot see a remote source's lifecycle: the
    /// incarnation counter is what makes an observer drop and redeclare its
    /// subscription across a source restart, and locally it only advances from
    /// local lifecycle events. Feeding the notification into the same fan-out
    /// is what makes a remote restart indistinguishable from a local one to
    /// the observer node.
    ///
    /// Idempotent in the sense that matters: a duplicate notification advances
    /// the generation again and redelivers, which the observer treats as a
    /// newer absolute state and converges on.
    pub async fn remote_source_reached_running(&self, core_node: &str, instance_id: &str) {
        self.source_reached_running(&SourceKey::new(core_node, instance_id), None)
            .await;
    }

    /// A source on another daemon stopped or died. Its observers are told the
    /// source went down; the generation is retained so a later restart is a
    /// strictly newer incarnation, exactly as for a local source.
    pub async fn remote_source_stopped(&self, core_node: &str, instance_id: &str) {
        let _guard = self.op_lock.lock().await;
        self.mark_source_down(&SourceKey::new(core_node, instance_id))
            .await;
    }

    /// Shared body of the local and remote "source reached Running" paths.
    ///
    /// `local_observer_instance` is `Some` only for a local instance, which may
    /// ALSO be an observer whose own sources are already live. A remote source
    /// is never an observer on this daemon, so that half is skipped.
    async fn source_reached_running(
        &self,
        source: &SourceKey,
        local_observer_instance: Option<&str>,
    ) {
        let _guard = self.op_lock.lock().await;

        // As a source: every time an instance reaches Running is a new
        // incarnation, so advance its generation unconditionally, the documented
        // "incarnation counter" semantics. Bumping even when no observer is
        // registered yet is what lets a source that started BEFORE its observer
        // (the `node run` ordering, where the source is an already-live instance
        // and the observer registers itself later) still hand that observer a
        // generation >= 1, distinct from the boot sentinel 0, when the observer
        // comes up and reads it. Under `stack launch` observers are registered
        // first, so this already held; making it unconditional makes the two
        // paths deliver identically regardless of start order.
        let is_local_source = source.core_node == self.updates.core_node_name();
        let live_instances = self.updates.node_stack().live_instance_ids_for_pairing();
        let deliveries = {
            let mut registry = self.registry.lock().unwrap();
            {
                let counter = registry
                    .source_generation
                    .entry(source.clone())
                    .or_insert(0);
                *counter += 1;
            }
            // A remote source has no local authority to ask, so its report of
            // reaching Running IS the record that it is up.
            if !is_local_source {
                registry.remote_live.insert(source.clone());
            }

            // Every slot observing this source, plus (when this instance is
            // itself an observer) every slot it declares, so it learns its whole
            // member set on the way up.
            let mut slots = Self::slots_observing(&registry, source);
            slots.extend(
                local_observer_instance
                    .into_iter()
                    .flat_map(|instance_id| Self::slots_of_observer(&registry, instance_id)),
            );

            self.assemble_deliveries(
                &registry,
                slots,
                &live_instances,
                Some(LivenessVerdict { source, live: true }),
            )
        };
        self.deliver_many(deliveries.into_iter().filter(|delivery| {
            // The instance this hook fires for is up by definition: it committed
            // to Running, which is why we are here. Reading its liveness off the
            // stack instead would make its own slots' delivery depend on when
            // the stack's live set happens to catch up.
            local_observer_instance == Some(delivery.slot.observer_instance_id.as_str())
                || live_instances.contains(&delivery.slot.observer_instance_id)
        }))
        .await;
    }

    /// Whether a source is currently up, from whichever authority owns it: the
    /// local node stack for a local source, and the last notification its
    /// owning daemon sent for a remote one. `verdict`, when it names this
    /// source, is the triggering event's own answer and wins over both.
    fn source_is_live(
        &self,
        registry: &Registry,
        source: &SourceKey,
        live_local_instances: &HashSet<String>,
        verdict: &Option<LivenessVerdict<'_>>,
    ) -> bool {
        if let Some(verdict) = verdict
            && verdict.source == source
        {
            return verdict.live;
        }
        if source.core_node == self.updates.core_node_name() {
            return live_local_instances.contains(&source.instance_id);
        }
        registry.remote_live.contains(source)
    }

    /// An instance stopped or was removed. If it is a source, every slot
    /// observing it is re-delivered with that member's `source_live` cleared
    /// (its pin stays in the set, at its position). If it is an observer, its
    /// records are dropped. The source's generation is retained so a later
    /// restart is a strictly newer incarnation.
    pub async fn on_instance_down(&self, instance_id: &str) {
        let _guard = self.op_lock.lock().await;

        let source = SourceKey::new(self.updates.core_node_name(), instance_id);
        // Drop this instance's own observer registrations. Only a local
        // instance can be an observer here, which is why this half has no
        // remote counterpart.
        self.registry
            .lock()
            .unwrap()
            .by_observer
            .remove(instance_id);
        self.mark_source_down(&source).await;
    }

    /// Records that a source went down and tells its live observers. Shared by
    /// the local down path and the remote notification. Caller holds
    /// `op_lock`.
    async fn mark_source_down(&self, source: &SourceKey) {
        let live_instances = self.updates.node_stack().live_instance_ids_for_pairing();
        let deliveries = {
            let mut registry = self.registry.lock().unwrap();
            // Assembled before the cleanup below, so the down source's members
            // still carry the generation they last ran under.
            let deliveries = self.assemble_deliveries(
                &registry,
                Self::slots_observing(&registry, source),
                &live_instances,
                Some(LivenessVerdict {
                    source,
                    live: false,
                }),
            );
            // Unconditionally, unlike the generation: whether anything still
            // observes this source has no bearing on whether it is up. A
            // no-op for a local source, which never gets a marker.
            registry.remote_live.remove(source);
            Self::forget_unobserved_source_locked(&mut registry, source);
            deliveries
        };
        self.deliver_many(
            deliveries
                .into_iter()
                .filter(|delivery| live_instances.contains(&delivery.slot.observer_instance_id)),
        )
        .await;
    }

    /// Every slot with a member observing `source`.
    ///
    /// One function for the up and down paths because they must reach exactly
    /// the same slots: a "went down" addressed to fewer slots than the matching
    /// "came up" leaves the difference permanently stale.
    /// A set, not a list: one slot may observe a source through several of the
    /// source's own participant slots, and it is still one delivery. Collecting
    /// into a `BTreeSet` is what makes that true here and at every caller, so
    /// unioning two of these needs no second deduplication.
    fn slots_observing(registry: &Registry, source: &SourceKey) -> BTreeSet<SlotKey> {
        registry
            .by_observer
            .iter()
            .flat_map(|(observer_id, records)| {
                records
                    .iter()
                    .filter(|record| &record.source() == source)
                    .map(move |record| SlotKey {
                        observer_instance_id: observer_id.clone(),
                        observer_link_id: record.observer_link_id.clone(),
                    })
            })
            .collect()
    }

    /// Every slot one observer instance declares a member for.
    fn slots_of_observer(registry: &Registry, observer_instance_id: &str) -> BTreeSet<SlotKey> {
        registry
            .by_observer
            .get(observer_instance_id)
            .into_iter()
            .flatten()
            .map(|record| SlotKey {
                observer_instance_id: observer_instance_id.to_string(),
                observer_link_id: record.observer_link_id.clone(),
            })
            .collect()
    }

    /// Materializes each slot's complete ordered member set, stamping every
    /// member with its own current generation and liveness. Registry order is
    /// plan order, so the set the node receives is the set the deployment
    /// wrote.
    fn assemble_deliveries(
        &self,
        registry: &Registry,
        slots: BTreeSet<SlotKey>,
        live_local_instances: &HashSet<String>,
        verdict: Option<LivenessVerdict<'_>>,
    ) -> Vec<Delivery> {
        slots
            .into_iter()
            .map(|slot| {
                let members = registry
                    .by_observer
                    .get(&slot.observer_instance_id)
                    .into_iter()
                    .flatten()
                    .filter(|record| record.observer_link_id == slot.observer_link_id)
                    .map(|record| {
                        let source = record.source();
                        ObservedMemberState {
                            source_generation: registry
                                .source_generation
                                .get(&source)
                                .copied()
                                .unwrap_or(0),
                            source_live: self.source_is_live(
                                registry,
                                &source,
                                live_local_instances,
                                &verdict,
                            ),
                            source: record.pin.clone(),
                        }
                    })
                    .collect();
                Delivery { slot, members }
            })
            .collect()
    }

    /// Keeps a source generation only while at least one observer may
    /// reconnect to it. This bounds the registry on churny daemons.
    fn forget_unobserved_source_locked(registry: &mut Registry, source: &SourceKey) {
        let still_observed = registry
            .by_observer
            .values()
            .flatten()
            .any(|record| &record.source() == source);
        if !still_observed {
            registry.source_generation.remove(source);
        }
    }

    /// Delivers independent absolute-state updates concurrently. Entity labels
    /// are resolved once per observer, even when it owns several slots.
    async fn deliver_many(&self, deliveries: impl IntoIterator<Item = Delivery>) {
        let mut by_observer: BTreeMap<String, Vec<Delivery>> = BTreeMap::new();
        for delivery in deliveries {
            by_observer
                .entry(delivery.slot.observer_instance_id.clone())
                .or_default()
                .push(delivery);
        }

        let mut resolved = Vec::new();
        for (observer_id, deliveries) in by_observer {
            match self.updates.resolve_target(&observer_id) {
                Ok(target) => resolved.push((target, deliveries)),
                Err(reason) => {
                    for delivery in deliveries {
                        Self::warn_delivery_failure(&delivery, &reason);
                    }
                }
            }
        }

        let futures = resolved.iter().flat_map(|(target, deliveries)| {
            deliveries
                .iter()
                .map(move |delivery| self.deliver(target, delivery))
        });
        join_all(futures).await;
    }

    /// One `observation_update` service call carrying the absolute state of one
    /// observer slot: its complete ordered member set. Best-effort: a stale
    /// reply is success (a newer absolute state already landed), any other
    /// failure is logged and swallowed.
    async fn deliver(&self, target: &SlotUpdateTarget, delivery: &Delivery) {
        if let Err(reason) = self.send_observation_update(target, delivery).await {
            Self::warn_delivery_failure(delivery, &reason);
        }
    }

    fn warn_delivery_failure(delivery: &Delivery, reason: &str) {
        warn!(
            "Best-effort observation delivery to '{}' slot `{}` ({} member(s)) failed: {reason}",
            delivery.slot.observer_instance_id,
            delivery.slot.observer_link_id,
            delivery.members.len(),
        );
    }

    async fn send_observation_update(
        &self,
        target: &SlotUpdateTarget,
        delivery: &Delivery,
    ) -> std::result::Result<(), String> {
        let request = ObservationUpdateRequest {
            link_id: delivery.slot.observer_link_id.clone(),
            sequence: self.updates.next_sequence(),
            members: delivery.members.clone(),
        };
        let payload = request.encode().map_err(|e| e.to_string())?;

        self.updates
            .send_to(
                target,
                OBSERVATION_UPDATE_SERVICE,
                payload,
                OBSERVATION_UPDATE_TIMEOUT,
                "observation_update rejected",
            )
            .await
    }
}
