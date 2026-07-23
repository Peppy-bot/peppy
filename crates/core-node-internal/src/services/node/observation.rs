//! The daemon's authority for observer-slot delivery: it holds the observer
//! registry and pushes each observer its resolved source pin over the
//! `observation_update` service, following the source instance's lifecycle.
//!
//! Observation is not a registry commit like pairing, so there is nothing to
//! reserve or revert: an observer passively taps a producer's role topics
//! without joining the pairing and without claiming any slot. The coordinator
//! only tracks who observes whom (so a source coming up can notify its
//! observers) and a per-source incarnation generation (so an observer drops and
//! redeclares its wire subscription when its source restarts). Delivery is
//! best-effort: a push that fails is logged, never fatal, because an observer
//! that misses its pin simply stays silent until the next lifecycle notify.
//!
//! Two counters ride each update, exactly as the wire type documents:
//! `sequence` (strictly increasing, seeded from unix-millis at daemon start)
//! rejects stale re-deliveries; `source_generation` advances only when the
//! source's incarnation changes (it reaches Running again) and is the sole
//! discriminator between old-source and new-source messages on the wire.

use core_node_api::encoding::ObservationTarget;
use daemon_config::launcher::PlannedObservation;
use futures::future::join_all;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::encoding::observation_update::ObservationUpdateRequest;
use peppylib::messaging::{OBSERVATION_UPDATE_SERVICE, ObservationPin, ProducerRef};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use super::common::{SlotUpdateClient, SlotUpdateTarget};

/// How long a single `observation_update` delivery may take before it is
/// treated as failed. The service is pre-setup (registered before the node's
/// ready signal), so a healthy observer answers promptly.
const OBSERVATION_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);

/// One observer slot's resolved source, as the daemon holds it.
#[derive(Debug, Clone)]
struct ObserverRecord {
    observer_link_id: String,
    source_instance_id: String,
    pin: ObservationPin,
}

/// The observer registry. Observer records are the source of truth; source
/// fan-out is derived from them in one pass when a lifecycle event arrives.
#[derive(Default)]
struct Registry {
    /// observer instance id → its observer-slot records.
    by_observer: BTreeMap<String, Vec<ObserverRecord>>,
    /// source instance id → its current incarnation generation. Advances only
    /// on the source reaching Running; never decreases (kept across the
    /// source's own down/up so a restart is a strictly newer generation).
    source_generation: BTreeMap<String, u64>,
}

#[derive(Debug)]
struct Delivery {
    observer_instance_id: String,
    record: ObserverRecord,
    source_generation: u64,
    source_live: bool,
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
                    source_instance_id: obs.source.instance_id.clone(),
                    pin: ObservationPin {
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
    /// observer id (a re-run) are cleared and its new ones merged in. The
    /// source always lives on this daemon (a stack is daemon-scoped), so its
    /// [`ProducerRef`] is stamped with this coordinator's `core_node`, exactly
    /// as the launcher stamps a stack-launch observation.
    ///
    /// Register BEFORE the instance reaches Running: the daemon's
    /// `on_instance_running` hook then finds these records and delivers each
    /// source pin whose source is already live, matching the launcher path.
    pub fn register_instance(
        &self,
        observer_instance_id: &str,
        observations: &BTreeMap<String, ObservationTarget>,
    ) {
        let mut registry = self.registry.lock().unwrap();
        Self::unregister_observer_locked(&mut registry, observer_instance_id);
        for (observer_link_id, target) in observations {
            Self::insert_record(
                &mut registry,
                observer_instance_id,
                ObserverRecord {
                    observer_link_id: observer_link_id.clone(),
                    source_instance_id: target.source_instance_id.clone(),
                    pin: ObservationPin {
                        producer: ProducerRef::new(
                            self.updates.core_node_name(),
                            &target.source_instance_id,
                        ),
                        source_link_id: target.source_link_id.clone(),
                    },
                },
            );
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
    /// generation advances and every live observer of it is re-delivered
    /// (source live). If it is itself an observer, each of its records whose
    /// source is currently live is delivered. Both can hold for one instance.
    pub async fn on_instance_running(&self, instance_id: &str) {
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
        let (source_deliveries, observer_deliveries) = {
            let mut registry = self.registry.lock().unwrap();
            let generation = {
                let counter = registry
                    .source_generation
                    .entry(instance_id.to_string())
                    .or_insert(0);
                *counter += 1;
                *counter
            };
            let source_deliveries: Vec<Delivery> = registry
                .by_observer
                .iter()
                .flat_map(|(observer_id, records)| {
                    records
                        .iter()
                        .filter(|record| record.source_instance_id == instance_id)
                        .cloned()
                        .map(|record| Delivery {
                            observer_instance_id: observer_id.clone(),
                            record,
                            source_generation: generation,
                            source_live: true,
                        })
                })
                .collect();
            let observer_deliveries: Vec<Delivery> = registry
                .by_observer
                .get(instance_id)
                .into_iter()
                .flatten()
                .cloned()
                .map(|record| Delivery {
                    source_generation: registry
                        .source_generation
                        .get(&record.source_instance_id)
                        .copied()
                        .unwrap_or(0),
                    observer_instance_id: instance_id.to_string(),
                    record,
                    source_live: true,
                })
                .collect();
            (source_deliveries, observer_deliveries)
        };
        let live_instances = self.updates.node_stack().live_instance_ids_for_pairing();
        self.deliver_many(
            source_deliveries
                .into_iter()
                .filter(|delivery| live_instances.contains(&delivery.observer_instance_id)),
        )
        .await;

        // As an observer: deliver each record whose source is already live.
        self.deliver_many(
            observer_deliveries
                .into_iter()
                .filter(|delivery| live_instances.contains(&delivery.record.source_instance_id)),
        )
        .await;
    }

    /// An instance stopped or was removed. If it is a source, its live
    /// observers are told the source went down (pin retained, `source_live`
    /// false). If it is an observer, its records are dropped. The source's
    /// generation is retained so a later restart is a strictly newer
    /// incarnation.
    pub async fn on_instance_down(&self, instance_id: &str) {
        let _guard = self.op_lock.lock().await;

        let deliveries = {
            let mut registry = self.registry.lock().unwrap();
            let generation = registry
                .source_generation
                .get(instance_id)
                .copied()
                .unwrap_or(0);
            let deliveries: Vec<Delivery> = registry
                .by_observer
                .iter()
                .flat_map(|(observer_id, records)| {
                    records
                        .iter()
                        .filter(|record| record.source_instance_id == instance_id)
                        .cloned()
                        .map(|record| Delivery {
                            observer_instance_id: observer_id.clone(),
                            record,
                            source_generation: generation,
                            source_live: false,
                        })
                })
                .collect();

            // Drop this instance's own observer registrations.
            registry.by_observer.remove(instance_id);

            // Keep a source generation only while at least one observer may
            // reconnect to it. This bounds the registry on churny daemons.
            let still_observed = registry
                .by_observer
                .values()
                .flatten()
                .any(|record| record.source_instance_id == instance_id);
            if !still_observed {
                registry.source_generation.remove(instance_id);
            }
            deliveries
        };
        let live_instances = self.updates.node_stack().live_instance_ids_for_pairing();
        self.deliver_many(
            deliveries
                .into_iter()
                .filter(|delivery| live_instances.contains(&delivery.observer_instance_id)),
        )
        .await;
    }

    /// Clears an observer for callers that already hold the registry lock
    /// (currently [`register_instance`] replacing a re-run's records).
    fn unregister_observer_locked(registry: &mut Registry, observer_id: &str) {
        registry.by_observer.remove(observer_id);
    }

    /// Delivers independent absolute-state updates concurrently. Entity labels
    /// are resolved once per observer, even when it owns several slots.
    async fn deliver_many(&self, deliveries: impl IntoIterator<Item = Delivery>) {
        let mut by_observer: BTreeMap<String, Vec<Delivery>> = BTreeMap::new();
        for delivery in deliveries {
            by_observer
                .entry(delivery.observer_instance_id.clone())
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

    /// One `observation_update` service call carrying the absolute source state
    /// of `record.observer_link_id` on the observer instance. Best-effort: a
    /// stale reply is success (a newer absolute state already landed), any
    /// other failure is logged and swallowed.
    async fn deliver(&self, target: &SlotUpdateTarget, delivery: &Delivery) {
        if let Err(reason) = self
            .send_observation_update(
                target,
                &delivery.record,
                delivery.source_generation,
                delivery.source_live,
            )
            .await
        {
            Self::warn_delivery_failure(delivery, &reason);
        }
    }

    fn warn_delivery_failure(delivery: &Delivery, reason: &str) {
        warn!(
            "Best-effort observation delivery to '{}' slot `{}` (source '{}') failed: {reason}",
            delivery.observer_instance_id,
            delivery.record.observer_link_id,
            delivery.record.source_instance_id
        );
    }

    async fn send_observation_update(
        &self,
        target: &SlotUpdateTarget,
        record: &ObserverRecord,
        source_generation: u64,
        source_live: bool,
    ) -> std::result::Result<(), String> {
        let request = ObservationUpdateRequest {
            link_id: record.observer_link_id.clone(),
            sequence: self.updates.next_sequence(),
            source: Some(record.pin.clone()),
            source_generation,
            source_live,
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
