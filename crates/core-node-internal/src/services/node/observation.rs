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
    pub async fn register_planned(&self, planned: &[PlannedObservation]) {
        let mut registry = self.registry.lock().unwrap();
        *registry = Registry::default();
        for obs in planned {
            registry
                .observers_of_source
                .entry(obs.source.instance_id.clone())
                .or_default()
                .insert(obs.observer_instance_id.clone());
            registry
                .by_observer
                .entry(obs.observer_instance_id.clone())
                .or_default()
                .push(ObserverRecord {
                    observer_link_id: obs.observer_link_id.clone(),
                    source_instance_id: obs.source.instance_id.clone(),
                    pin: ObservationPin {
                        producer: obs.source.clone(),
                        source_link_id: obs.source_link_id.clone(),
                    },
                });
        }
    }

    /// An instance reached Running. If it is a source, its incarnation
    /// generation advances and every live observer of it is re-delivered
    /// (source live). If it is itself an observer, each of its records whose
    /// source is currently live is delivered. Both can hold for one instance.
    pub async fn on_instance_running(&self, instance_id: &str) {
        let _guard = self.op_lock.lock().await;

        // As a source: bump generation and fan out to live observers.
        let (source_observers, generation) = {
            let mut registry = self.registry.lock().unwrap();
            match registry.observers_of_source.get(instance_id).cloned() {
                Some(observers) if !observers.is_empty() => {
                    let generation = registry
                        .source_generation
                        .entry(instance_id.to_string())
                        .or_insert(0);
                    *generation += 1;
                    (observers, *generation)
                }
                _ => (BTreeSet::new(), 0),
            }
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
