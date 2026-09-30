//! Per-resource runtime state: the update-rate gate, the snapshot store
//! read by `resources/read`, the readers that wait for the next message,
//! and the event channel behind subscription notifications.
//!
//! A read answers with a message the endpoint receives after the read
//! arrived: while a reader waits, the gate admits the next message
//! whatever the interval, the publish stores it and wakes the readers, and
//! every reader that waited gets that one message. Only a message the
//! interval admitted sends `ResourceUpdated`, so subscriptions keep their
//! `max_hz` rate. The wait is bounded by the freshness policy's
//! `max_age_ms` of wall time; a topic that stays silent that long answers
//! as a read of the stored snapshot does.

use crate::clock::Clock;
use crate::error::PublishError;
use crate::representation::{SnapshotContent, apply_content_policies};
use peppy_mcp_catalog::ResourceEntry;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

/// An event the subscription forwarder relays to listening clients: a
/// resource took a new snapshot, or the resource list changed because a
/// robot joined or left a per-robot surface.
#[derive(Debug, Clone)]
pub(crate) enum CatalogEvent {
    ResourceUpdated { uri: String },
    ResourceListChanged,
}

/// The latest policy-approved snapshot of one exposed topic.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// The content a read serves, after representation and size policies.
    pub(crate) content: SnapshotContent,
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
    pub(crate) content: SnapshotContent,
    /// Milliseconds until the freshness policy would report this snapshot
    /// stale; doubles as the read result's `ttlMs` hint.
    pub(crate) remaining_fresh_ms: u64,
}

pub(crate) struct ResourceState {
    pub(crate) entry: ResourceEntry,
    /// Minimum nanoseconds between admitted messages, from `update.max_hz`.
    min_interval_nanos: u64,
    /// When the gate last admitted a message by the interval.
    gate: Mutex<Option<u64>>,
    snapshot: RwLock<Option<Snapshot>>,
    /// How many readers wait for the next message: while any does, the
    /// gate admits it whatever the interval.
    waiting: AtomicUsize,
    /// Wakes the waiting readers once a message is stored.
    arrived: Notify,
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
            waiting: AtomicUsize::new(0),
            arrived: Notify::new(),
        }
    }

    fn admit(&self, now_nanos: u64) -> Option<AdmitToken> {
        let mut gate = self.gate.lock().expect("gate lock is never poisoned");
        let interval_passed = match *gate {
            Some(last_admit) => now_nanos.saturating_sub(last_admit) >= self.min_interval_nanos,
            None => true,
        };
        if interval_passed {
            *gate = Some(now_nanos);
            return Some(AdmitToken {
                taken_at_nanos: now_nanos,
                admitted_by: Admission::Interval,
            });
        }
        if self.waiting.load(Ordering::SeqCst) > 0 {
            return Some(AdmitToken {
                taken_at_nanos: now_nanos,
                admitted_by: Admission::Reader,
            });
        }
        None
    }

    fn store(&self, snapshot: Snapshot) {
        *self
            .snapshot
            .write()
            .expect("snapshot lock is never poisoned") = Some(snapshot);
        self.arrived.notify_waiters();
    }

    /// The snapshot a read that arrives now answers with: the first message
    /// the endpoint receives after this call, or, when none arrives within
    /// `max_age_ms` of wall time, the stored snapshot as [`Self::snapshot_for_read`]
    /// serves it. A resource with no snapshot, or with a stale one, is
    /// refused at once: the stored snapshot must pass the freshness policy
    /// before the wait, and again after it.
    pub(crate) async fn next_snapshot(&self, clock: &Clock) -> Result<SnapshotView, ReadRefusal> {
        self.snapshot_for_read(clock.now_nanos())?;
        let max_wait = Duration::from_millis(self.entry.policies.freshness.max_age_ms.get());
        // The interest is registered before the count opens the gate, so a
        // message admitted for this reader cannot be stored before the
        // reader listens for it.
        let arrived = self.arrived.notified();
        tokio::pin!(arrived);
        arrived.as_mut().enable();
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let waited = tokio::time::timeout(max_wait, arrived).await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        let _ = waited;
        self.snapshot_for_read(clock.now_nanos())
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
            content: snapshot.content.clone(),
            remaining_fresh_ms: max_age_ms - age_ms,
        })
    }
}

/// Proof that the update-rate gate admitted a message; only
/// [`ResourceIngest::admit`] mints one, so a publish cannot bypass the gate.
#[derive(Debug)]
pub struct AdmitToken {
    taken_at_nanos: u64,
    admitted_by: Admission,
}

/// What opened the gate for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// The interval since the last message the interval admitted has
    /// passed: the message is one of the `max_hz` stream, and its
    /// subscribers are told of it.
    Interval,
    /// A reader waits for the next message: the message answers the
    /// readers and no subscriber is told of it.
    Reader,
}

/// The feed a topic pump pushes decoded messages through. Handed out by
/// [`ExposureServer::ingest`](crate::ExposureServer::ingest).
#[derive(Clone)]
pub struct ResourceIngest {
    pub(crate) state: Arc<ResourceState>,
    pub(crate) events: broadcast::Sender<CatalogEvent>,
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
    /// snapshot, wakes the readers that wait for it and, for a message the
    /// interval admitted, notifies subscribed clients. On refusal the
    /// previous snapshot stays current and ages toward staleness.
    pub fn publish(&self, token: AdmitToken, value: Value) -> Result<(), PublishError> {
        let content = apply_content_policies(self.state.entry.policies.content(), value)?;
        self.state.store(Snapshot {
            content,
            taken_at_nanos: token.taken_at_nanos,
        });
        if token.admitted_by == Admission::Interval {
            // Send fails only when nobody listens, which is fine.
            let _ = self.events.send(CatalogEvent::ResourceUpdated {
                uri: self.state.entry.uri.clone(),
            });
        }
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
        assert_eq!(view.content.document, "{\"battery\":87}");
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
        assert_eq!(view.content.document, "{\"status\":\"ok\"}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_answers_with_the_first_message_after_it_whatever_the_interval() {
        let (ingest, nanos) = ingest_with_clock();
        let mut events = ingest.events.subscribe();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        let _ = events.try_recv().expect("the first publish notified");

        // The read arrives 100 ms after the first message, well inside the
        // 500 ms interval of the 2 Hz gate.
        nanos.store(100 * MS, Ordering::SeqCst);
        let reader = {
            let state = Arc::clone(&ingest.state);
            let clock = ingest.clock.clone();
            tokio::spawn(async move { state.next_snapshot(&clock).await })
        };
        tokio::task::yield_now().await;
        assert!(ingest.admit().is_some(), "a waiting reader opens the gate");
        nanos.store(150 * MS, Ordering::SeqCst);
        let token = ingest.admit().expect("gate open for the reader");
        ingest
            .publish(token, json!({ "battery": 2 }))
            .expect("publishes");
        let view = reader.await.expect("reader task").expect("fresh");
        assert_eq!(view.content.document, "{\"battery\":2}");
        assert!(
            events.try_recv().is_err(),
            "a message admitted for a reader alone tells no subscriber"
        );
        assert!(
            ingest.admit().is_none(),
            "with no reader waiting the interval gates again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn readers_that_wait_together_get_the_same_message() {
        let (ingest, nanos) = ingest_with_clock();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        nanos.store(100 * MS, Ordering::SeqCst);
        let read = || {
            let state = Arc::clone(&ingest.state);
            let clock = ingest.clock.clone();
            tokio::spawn(async move { state.next_snapshot(&clock).await })
        };
        let (first, second) = (read(), read());
        tokio::task::yield_now().await;
        assert_eq!(ingest.state.waiting.load(Ordering::SeqCst), 2);
        let token = ingest.admit().expect("gate open for the readers");
        ingest
            .publish(token, json!({ "battery": 2 }))
            .expect("publishes");
        for reader in [first, second] {
            let view = reader.await.expect("reader task").expect("fresh");
            assert_eq!(view.content.document, "{\"battery\":2}");
        }
        assert_eq!(ingest.state.waiting.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_topic_answers_the_stored_snapshot_after_the_bound_while_it_is_fresh() {
        let (ingest, nanos) = ingest_with_clock();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        nanos.store(500 * MS, Ordering::SeqCst);
        // Nothing arrives: the wait runs out after max_age_ms (2000 ms) and
        // the stored snapshot, still fresh, answers.
        let view = ingest
            .state
            .next_snapshot(&ingest.clock)
            .await
            .expect("the stored snapshot is fresh");
        assert_eq!(view.content.document, "{\"battery\":1}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_topic_whose_snapshot_went_stale_during_the_wait_is_refused() {
        let (ingest, nanos) = ingest_with_clock();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        nanos.store(1_500 * MS, Ordering::SeqCst);
        let reader = {
            let state = Arc::clone(&ingest.state);
            let clock = ingest.clock.clone();
            tokio::spawn(async move { state.next_snapshot(&clock).await })
        };
        // The clock moves past the freshness bound while the reader waits.
        tokio::task::yield_now().await;
        nanos.store(2_001 * MS, Ordering::SeqCst);
        let refusal = reader
            .await
            .expect("reader task")
            .expect_err("the snapshot went stale during the wait");
        assert_eq!(
            refusal,
            ReadRefusal::Stale {
                age_ms: 2001,
                max_age_ms: 2000
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_resource_without_a_snapshot_or_with_a_stale_one_is_refused_without_waiting() {
        let (ingest, nanos) = ingest_with_clock();
        assert!(matches!(
            ingest.state.next_snapshot(&ingest.clock).await,
            Err(ReadRefusal::Unavailable)
        ));
        assert_eq!(ingest.state.waiting.load(Ordering::SeqCst), 0);
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 1 }))
            .expect("publishes");
        nanos.store(3_000 * MS, Ordering::SeqCst);
        assert!(matches!(
            ingest.state.next_snapshot(&ingest.clock).await,
            Err(ReadRefusal::Stale { .. })
        ));
    }

    #[test]
    fn each_publish_emits_a_resource_updated_event() {
        let (ingest, _) = ingest_with_clock();
        let mut receiver = ingest.events.subscribe();
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");
        let CatalogEvent::ResourceUpdated { uri } =
            receiver.try_recv().expect("one event is queued")
        else {
            panic!("a publish is a resource update");
        };
        assert_eq!(uri, "peppy://resource/front_camera.status");
    }
}
