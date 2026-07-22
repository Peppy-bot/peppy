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

use config::runtime::Name;
use core_node_api::encoding::ObservationTarget;
use daemon_config::launcher::PlannedObservation;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::encoding::observation_update::{ObservationUpdateRequest, ObservationUpdateResponse};
use peppylib::messaging::{
    OBSERVATION_UPDATE_SERVICE, ObservationPin, ProducerRef, SenderTarget, ServiceMessenger,
    ServiceTarget,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::warn;

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

/// The observer registry. Keyed both ways so a newly-running observer finds its
/// sources and a newly-running source finds its observers.
#[derive(Default)]
struct Registry {
    /// observer instance id → its observer-slot records.
    by_observer: BTreeMap<String, Vec<ObserverRecord>>,
    /// source instance id → the observer instance ids watching it.
    observers_of_source: BTreeMap<String, BTreeSet<String>>,
    /// source instance id → its current incarnation generation. Advances only
    /// on the source reaching Running; never decreases (kept across the
    /// source's own down/up so a restart is a strictly newer generation).
    source_generation: BTreeMap<String, u64>,
}

pub struct ObservationCoordinator {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    /// This daemon's core node name: the identity `observation_update` calls
    /// are sent under (stacks are daemon-scoped, so every source lives here).
    core_node_name: String,
    /// The daemon's own instance id, used as the caller identity on service
    /// calls.
    caller_instance_id: String,
    /// Serializes lifecycle notifications so a source's generation bump and its
    /// fan-out delivery cannot interleave with a concurrent notify.
    op_lock: tokio::sync::Mutex<()>,
    /// Monotonic transport sequence, seeded from unix-millis at daemon start.
    seq: AtomicU64,
    registry: std::sync::Mutex<Registry>,
}

impl ObservationCoordinator {
    pub fn new(
        node_stack: Arc<NodeStack>,
        messenger: MessengerHandle,
        core_node_name: impl Into<String>,
        caller_instance_id: impl Into<String>,
    ) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1);
        Self {
            node_stack,
            messenger,
            core_node_name: core_node_name.into(),
            caller_instance_id: caller_instance_id.into(),
            op_lock: tokio::sync::Mutex::new(()),
            seq: AtomicU64::new(seed),
            registry: std::sync::Mutex::new(Registry::default()),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Records every planned observation so later lifecycle notifications can
    /// find them. Called once by the launcher before any instance starts, so a
    /// source that comes up first still finds its waiting observers. A launch
    /// replaces the whole stack, so this replaces the registry too (the
    /// previous stack's observers are gone); passing an empty slice clears it.
    /// Drops the entire observer registry. The observation twin of
    /// `node_stack.reset()` clearing the pairing registry: `stack reset` tears
    /// the whole stack down at once, so without this a re-run of the same
    /// instance ids inherits stale observations and a source coming back up
    /// would deliver a pin the user never linked this time. `stack launch` does
    /// not need it because [`register_planned`] already replaces the registry.
    pub async fn clear(&self) {
        *self.registry.lock().unwrap() = Registry::default();
    }

    pub async fn register_planned(&self, planned: &[PlannedObservation]) {
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
    pub async fn register_instance(
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
                            &self.core_node_name,
                            &target.source_instance_id,
                        ),
                        source_link_id: target.source_link_id.clone(),
                    },
                },
            );
        }
    }

    /// Inserts one resolved observer record into both directions of the
    /// registry. Shared by [`register_planned`] and [`register_instance`].
    fn insert_record(registry: &mut Registry, observer_instance_id: &str, record: ObserverRecord) {
        registry
            .observers_of_source
            .entry(record.source_instance_id.clone())
            .or_default()
            .insert(observer_instance_id.to_string());
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
        let (source_observers, generation) = {
            let mut registry = self.registry.lock().unwrap();
            let generation = {
                let counter = registry
                    .source_generation
                    .entry(instance_id.to_string())
                    .or_insert(0);
                *counter += 1;
                *counter
            };
            let observers = registry
                .observers_of_source
                .get(instance_id)
                .cloned()
                .unwrap_or_default();
            (observers, generation)
        };
        for observer_id in &source_observers {
            for record in self.records_for_source(observer_id, instance_id) {
                if self.node_stack.instance_is_live_for_pairing(observer_id) {
                    self.deliver(observer_id, &record, generation, true).await;
                }
            }
        }

        // As an observer: deliver each record whose source is already live.
        for record in self.records_of(instance_id) {
            if self
                .node_stack
                .instance_is_live_for_pairing(&record.source_instance_id)
            {
                let generation = self.generation_of(&record.source_instance_id);
                self.deliver(instance_id, &record, generation, true).await;
            }
        }
    }

    /// An instance stopped or was removed. If it is a source, its live
    /// observers are told the source went down (pin retained, `source_live`
    /// false). If it is an observer, its records are dropped. The source's
    /// generation is retained so a later restart is a strictly newer
    /// incarnation.
    pub async fn on_instance_down(&self, instance_id: &str) {
        let _guard = self.op_lock.lock().await;

        let source_observers = {
            let registry = self.registry.lock().unwrap();
            registry
                .observers_of_source
                .get(instance_id)
                .cloned()
                .unwrap_or_default()
        };
        let generation = self.generation_of(instance_id);
        for observer_id in &source_observers {
            for record in self.records_for_source(observer_id, instance_id) {
                if self.node_stack.instance_is_live_for_pairing(observer_id) {
                    self.deliver(observer_id, &record, generation, false).await;
                }
            }
        }

        // Drop this instance's own observer registrations.
        self.unregister_observer(instance_id);

        // Prune this instance's incarnation counter once nothing observes it:
        // an unobserved source's generation is meaningless, and now that every
        // instance reaching Running bumps a generation, keeping one entry per
        // instance that ever ran would grow the registry unbounded on a churny
        // `node run` daemon. If observers remain (they may reconnect when the
        // source restarts) the counter is retained so that restart is a
        // strictly newer incarnation than what those observers last saw.
        {
            let mut registry = self.registry.lock().unwrap();
            let still_observed = registry
                .observers_of_source
                .get(instance_id)
                .is_some_and(|observers| !observers.is_empty());
            if !still_observed {
                registry.observers_of_source.remove(instance_id);
                registry.source_generation.remove(instance_id);
            }
        }
    }

    fn generation_of(&self, source_instance_id: &str) -> u64 {
        self.registry
            .lock()
            .unwrap()
            .source_generation
            .get(source_instance_id)
            .copied()
            .unwrap_or(0)
    }

    /// All observer records held for `observer_id`.
    fn records_of(&self, observer_id: &str) -> Vec<ObserverRecord> {
        self.registry
            .lock()
            .unwrap()
            .by_observer
            .get(observer_id)
            .cloned()
            .unwrap_or_default()
    }

    /// The subset of `observer_id`'s records whose source is `source_id`.
    fn records_for_source(&self, observer_id: &str, source_id: &str) -> Vec<ObserverRecord> {
        self.records_of(observer_id)
            .into_iter()
            .filter(|r| r.source_instance_id == source_id)
            .collect()
    }

    fn unregister_observer(&self, observer_id: &str) {
        let mut registry = self.registry.lock().unwrap();
        Self::unregister_observer_locked(&mut registry, observer_id);
    }

    /// Body of [`unregister_observer`] for callers that already hold the
    /// registry lock (e.g. [`register_instance`] clearing a re-run's records).
    fn unregister_observer_locked(registry: &mut Registry, observer_id: &str) {
        if let Some(records) = registry.by_observer.remove(observer_id) {
            for record in records {
                if let Some(observers) = registry
                    .observers_of_source
                    .get_mut(&record.source_instance_id)
                {
                    observers.remove(observer_id);
                }
            }
        }
    }

    /// One `observation_update` service call carrying the absolute source state
    /// of `record.observer_link_id` on the observer instance. Best-effort: a
    /// stale reply is success (a newer absolute state already landed), any
    /// other failure is logged and swallowed.
    async fn deliver(
        &self,
        observer_instance_id: &str,
        record: &ObserverRecord,
        source_generation: u64,
        source_live: bool,
    ) {
        if let Err(reason) = self
            .send_observation_update(observer_instance_id, record, source_generation, source_live)
            .await
        {
            warn!(
                "Best-effort observation delivery to '{}' slot `{}` (source '{}') failed: {reason}",
                observer_instance_id, record.observer_link_id, record.source_instance_id
            );
        }
    }

    async fn send_observation_update(
        &self,
        observer_instance_id: &str,
        record: &ObserverRecord,
        source_generation: u64,
        source_live: bool,
    ) -> std::result::Result<(), String> {
        let instance_name = Name::new(observer_instance_id)
            .map_err(|e| format!("invalid instance id: {e}"))?;
        let (node_name, node_tag) = self
            .node_stack
            .find_entity_label_for_instance_id_any_state(&instance_name)
            .ok_or_else(|| "observer instance is no longer tracked".to_string())?;

        let request = ObservationUpdateRequest {
            link_id: record.observer_link_id.clone(),
            sequence: self.next_seq(),
            source: Some(record.pin.clone()),
            source_generation,
            source_live,
        };
        let payload = request.encode().map_err(|e| e.to_string())?;

        let reply = ServiceMessenger::poll(
            &self.messenger,
            &self.core_node_name,
            &self.caller_instance_id,
            SenderTarget::node(&node_name, &node_tag).map_err(|e| e.to_string())?,
            OBSERVATION_UPDATE_SERVICE,
            ServiceTarget::Producer(&ProducerRef::new(
                &self.core_node_name,
                observer_instance_id,
            )),
            payload,
            OBSERVATION_UPDATE_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;

        let response =
            ObservationUpdateResponse::decode(&reply.payload_bytes()).map_err(|e| e.to_string())?;
        if response.accepted || response.stale_sequence {
            Ok(())
        } else {
            Err(if response.message.is_empty() {
                "observation_update rejected".to_string()
            } else {
                response.message
            })
        }
    }
}
