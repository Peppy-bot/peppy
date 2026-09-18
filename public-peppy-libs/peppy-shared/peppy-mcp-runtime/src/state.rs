//! Per-resource runtime state: the update-rate gate, the snapshot store
//! read by `resources/read`, and the event channel behind subscription
//! notifications.

use crate::clock::Clock;
use crate::error::PublishError;
use crate::representation::apply_topic_policies;
use peppy_mcp_catalog::ResourceEntry;
use serde_json::Value;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::broadcast;

/// The resource-updated event the subscription forwarder relays to
/// listening clients. The catalog is fixed for the life of the server, so
/// no other event kind can exist.
#[derive(Debug, Clone)]
pub(crate) struct ResourceUpdated {
    pub(crate) uri: String,
}

/// The latest policy-approved snapshot of one exposed topic.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// The final serialized content a read serves, after representation and
    /// size policies.
    pub(crate) serialized: String,
    pub(crate) taken_at_nanos: u64,
}

/// Why a read cannot serve a snapshot right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadRefusal {
    /// Nothing has been published since the server started.
    Unavailable,
    /// The stored snapshot is older than the freshness policy allows.
    Stale { age_ms: u64, max_age_ms: u64 },
}

/// A snapshot cleared for serving, with the freshness it has left.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotView {
    pub(crate) serialized: String,
    /// Milliseconds until the freshness policy would report this snapshot
    /// stale; doubles as the read result's `ttlMs` hint.
    pub(crate) remaining_fresh_ms: u64,
}

pub(crate) struct ResourceState {
    pub(crate) entry: ResourceEntry,
    /// Minimum nanoseconds between admitted messages, from `update.max_hz`.
    min_interval_nanos: u64,
    /// When the gate last admitted a message.
    gate: Mutex<Option<u64>>,
    snapshot: RwLock<Option<Snapshot>>,
}

impl ResourceState {
    pub(crate) fn new(entry: ResourceEntry) -> Self {
        let min_interval_nanos =
            (1_000_000_000f64 / entry.policies.update.max_hz.get()).round() as u64;
        Self {
            entry,
            min_interval_nanos,
            gate: Mutex::new(None),
            snapshot: RwLock::new(None),
        }
    }

    fn admit(&self, now_nanos: u64) -> Option<AdmitToken> {
        let mut gate = self.gate.lock().expect("gate lock is never poisoned");
        match *gate {
            Some(last_admit) if now_nanos.saturating_sub(last_admit) < self.min_interval_nanos => {
                None
            }
            _ => {
                *gate = Some(now_nanos);
                Some(AdmitToken {
                    taken_at_nanos: now_nanos,
                })
            }
        }
    }

    fn store(&self, snapshot: Snapshot) {
        *self
            .snapshot
            .write()
            .expect("snapshot lock is never poisoned") = Some(snapshot);
    }

    pub(crate) fn snapshot_for_read(&self, now_nanos: u64) -> Result<SnapshotView, ReadRefusal> {
        let snapshot = self
            .snapshot
            .read()
            .expect("snapshot lock is never poisoned");
        let Some(snapshot) = snapshot.as_ref() else {
            return Err(ReadRefusal::Unavailable);
        };
        let age_ms = now_nanos.saturating_sub(snapshot.taken_at_nanos) / 1_000_000;
        let max_age_ms = self.entry.policies.freshness.max_age_ms.get();
        if age_ms > max_age_ms {
            return Err(ReadRefusal::Stale { age_ms, max_age_ms });
        }
        Ok(SnapshotView {
            serialized: snapshot.serialized.clone(),
            remaining_fresh_ms: max_age_ms - age_ms,
        })
    }
}

/// Proof that the update-rate gate admitted a message; only
/// [`ResourceIngest::admit`] mints one, so a publish cannot bypass the gate.
#[derive(Debug)]
pub struct AdmitToken {
    taken_at_nanos: u64,
}

/// The feed a topic pump pushes decoded messages through. Handed out by
/// [`ExposureServer::ingest`](crate::ExposureServer::ingest).
#[derive(Clone)]
pub struct ResourceIngest {
    pub(crate) state: Arc<ResourceState>,
    pub(crate) events: broadcast::Sender<ResourceUpdated>,
    pub(crate) clock: Clock,
}

impl ResourceIngest {
    /// Applies the update-rate gate. Call this before decoding the message
    /// body: a `None` means the message is dropped by `max_hz` and no
    /// decode or transcode cost should be paid for it.
    pub fn admit(&self) -> Option<AdmitToken> {
        self.state.admit(self.clock.now_nanos())
    }

    /// Applies the representation and size policies to the admitted
    /// message's canonical JSON and, when they pass, makes it the current
    /// snapshot and notifies subscribed clients. On refusal the previous
    /// snapshot stays current and ages toward staleness.
    pub fn publish(&self, token: AdmitToken, mut value: Value) -> Result<(), PublishError> {
        let serialized = apply_topic_policies(&self.state.entry.policies, &mut value)?;
        self.state.store(Snapshot {
            serialized,
            taken_at_nanos: token.taken_at_nanos,
        });
        // Send fails only when nobody listens, which is fine.
        let _ = self.events.send(ResourceUpdated {
            uri: self.state.entry.uri.clone(),
        });
        Ok(())
    }

    /// The public resource name this ingest feeds.
    pub fn resource_name(&self) -> &str {
        &self.state.entry.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::test_support::manual_clock;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn status_entry() -> ResourceEntry {
        serde_json::from_value(json!({
            "name": "front_camera.status",
            "uri": "peppy://resource/front_camera.status",
            "description": "Latest camera status.",
            "target": "front_camera",
            "member": "camera_status",
            "policies": {
                "freshness": { "max_age_ms": 2000 },
                "update": { "max_hz": 2.0 },
            },
            "schema": { "type": "object" },
        }))
        .expect("valid resource entry")
    }

    fn ingest_with_clock() -> (ResourceIngest, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let (clock, nanos) = manual_clock();
        let (events, _) = broadcast::channel(16);
        let ingest = ResourceIngest {
            state: Arc::new(ResourceState::new(status_entry())),
            events,
            clock,
        };
        (ingest, nanos)
    }

    const MS: u64 = 1_000_000;

    #[test]
    fn the_gate_admits_at_most_max_hz() {
        let (ingest, nanos) = ingest_with_clock();
        assert!(ingest.admit().is_some(), "first message is always admitted");
        nanos.store(499 * MS, Ordering::SeqCst);
        assert!(ingest.admit().is_none(), "2 Hz means 500 ms between admits");
        nanos.store(500 * MS, Ordering::SeqCst);
        assert!(
            ingest.admit().is_some(),
            "the full interval reopens the gate"
        );
        nanos.store(999 * MS, Ordering::SeqCst);
        assert!(
            ingest.admit().is_none(),
            "the interval restarts at each admit"
        );
    }

    #[test]
    fn reads_report_unavailable_then_fresh_then_stale() {
        let (ingest, nanos) = ingest_with_clock();
        assert!(
            matches!(
                ingest.state.snapshot_for_read(0),
                Err(ReadRefusal::Unavailable)
            ),
            "nothing published yet"
        );

        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");

        nanos.store(1_500 * MS, Ordering::SeqCst);
        let view = ingest
            .state
            .snapshot_for_read(nanos.load(Ordering::SeqCst))
            .expect("1500 ms old is within max_age_ms 2000");
        assert_eq!(view.serialized, "{\"battery\":87}");
        assert_eq!(view.remaining_fresh_ms, 500);

        nanos.store(2_001 * MS, Ordering::SeqCst);
        let refusal = ingest
            .state
            .snapshot_for_read(nanos.load(Ordering::SeqCst))
            .expect_err("2001 ms old exceeds max_age_ms 2000");
        assert_eq!(
            refusal,
            ReadRefusal::Stale {
                age_ms: 2001,
                max_age_ms: 2000
            }
        );
    }

    #[test]
    fn snapshot_age_is_measured_from_admission_not_from_now() {
        let (ingest, nanos) = ingest_with_clock();
        let token = ingest.admit().expect("gate open");
        nanos.store(600 * MS, Ordering::SeqCst);
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        let view = ingest.state.snapshot_for_read(600 * MS).expect("fresh");
        assert_eq!(
            view.remaining_fresh_ms, 1400,
            "age counts from the admit at t=0"
        );
    }

    #[test]
    fn a_refused_publish_keeps_the_previous_snapshot() {
        let (clock, nanos) = manual_clock();
        let (events, _) = broadcast::channel(16);
        let mut entry = status_entry();
        entry.policies.max_result_bytes = Some(std::num::NonZeroU64::new(32).expect("non-zero"));
        entry.policies.on_oversize =
            Some(serde_json::from_value(json!("reject")).expect("valid policy"));
        let ingest = ResourceIngest {
            state: Arc::new(ResourceState::new(entry)),
            events,
            clock,
        };

        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "status": "ok" }))
            .expect("small snapshot fits");

        nanos.store(500 * MS, Ordering::SeqCst);
        let token = ingest.admit().expect("gate reopened");
        let error = ingest
            .publish(token, json!({ "status": "y".repeat(64) }))
            .expect_err("oversize snapshot is rejected");
        assert!(matches!(error, PublishError::Oversize { .. }));

        let view = ingest
            .state
            .snapshot_for_read(500 * MS)
            .expect("previous snapshot serves");
        assert_eq!(view.serialized, "{\"status\":\"ok\"}");
    }

    #[test]
    fn each_publish_emits_a_resource_updated_event() {
        let (ingest, _) = ingest_with_clock();
        let mut receiver = ingest.events.subscribe();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");
        let ResourceUpdated { uri } = receiver.try_recv().expect("one event is queued");
        assert_eq!(uri, "peppy://resource/front_camera.status");
    }
}
