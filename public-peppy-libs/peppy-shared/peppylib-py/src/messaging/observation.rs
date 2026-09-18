//! Python bindings for the observer runtime surface: [`PyObservedSource`] (one
//! observed pairing of an observer slot), [`PyObservationSlot`] (read a scalar
//! slot's source), [`PyObservationSlotSet`] (read a multi-member slot's whole
//! set), and [`PyObservedSubscription`] (receive the observed sources'
//! publishes on one topic, yielded as `(source, message)`).

use super::pairing::PyPeerInfo;
use super::target::PyProducerRef;
use super::topics::PyTopicMessage;
use peppylib::messaging::ObservedSource;
use peppylib::runtime::{ObservationSlot, ObservationSlotSet, ObservedTopicSubscription};
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// One pairing an observer slot observes: the observed instance's full
/// `(core_node, instance_id)` wire address plus the producer-side link_id of the
/// observed pairing slot, and the pair's other end when the plan named the
/// pair by it. Returned by `ObservationSlot.source()` and
/// `ObservationSlotSet.sources()`, and tagged onto every message an
/// `ObservedSubscription` yields; it is the member's full identity, so members
/// sharing one instance, or one slot, stay distinct. `frozen, eq, hash` make
/// it usable directly as a `dict` key for per-member demux, mirroring the Rust
/// `HashMap<ObservedSource, _>` idiom. Purely local configuration state; there
/// is no health-derived helper, because a third node's health is not knowable
/// here.
#[pyclass(name = "ObservedSource", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyObservedSource {
    pub(crate) inner: ObservedSource,
}

#[pymethods]
impl PyObservedSource {
    #[new]
    #[pyo3(signature = (producer, source_link_id, peer = None))]
    fn new(
        producer: PyProducerRef,
        source_link_id: String,
        peer: Option<PyRef<'_, PyPeerInfo>>,
    ) -> Self {
        Self {
            inner: ObservedSource {
                producer: producer.into_inner(),
                source_link_id,
                peer: peer.map(|peer| peer.inner.clone()),
            },
        }
    }

    /// The observed source instance's full wire address.
    #[getter]
    fn producer(&self) -> PyProducerRef {
        PyProducerRef::from(self.inner.producer.clone())
    }

    /// The producer-side link_id of the observed pairing slot.
    #[getter]
    fn source_link_id(&self) -> &str {
        &self.inner.source_link_id
    }

    /// The pair's other end when the plan named the pair by it: the peer the
    /// source publishes to and the link_id of the peer's slot. `None` when
    /// this member observes every pair of the source's slot.
    #[getter]
    fn peer(&self) -> Option<PyPeerInfo> {
        self.inner.peer.clone().map(PyPeerInfo::from)
    }

    fn __repr__(&self) -> String {
        let peer = match &self.inner.peer {
            Some(peer) => format!(
                "PeerInfo(producer=ProducerRef({:?}, {:?}), peer_link_id={:?})",
                peer.producer.core_node, peer.producer.instance_id, peer.peer_link_id
            ),
            None => "None".to_string(),
        };
        format!(
            "ObservedSource(producer=ProducerRef({:?}, {:?}), source_link_id={:?}, peer={peer})",
            self.inner.producer.core_node,
            self.inner.producer.instance_id,
            self.inner.source_link_id
        )
    }
}

impl From<ObservedSource> for PyObservedSource {
    fn from(inner: ObservedSource) -> Self {
        Self { inner }
    }
}

/// Handle onto a scalar observer slot's live observation state (a
/// `cardinality: "one"` or `cardinality: "zero_or_one"` slot), obtained via
/// `node_runner.observation_slot(link_id)`. Each scalar cardinality reads
/// through the accessor it declares: `sole_source()` for a `one` slot, which
/// always observes a pairing, and `source()` for a `zero_or_one` slot, which
/// answers `None` where the deployment wrote it vacant. Multi-member slots are
/// read through [`PyObservationSlotSet`] instead.
#[pyclass(name = "ObservationSlot")]
pub struct PyObservationSlot {
    pub(crate) inner: ObservationSlot,
}

#[pymethods]
impl PyObservationSlot {
    /// The observed source of a `zero_or_one` slot, or `None` where the
    /// deployment wrote it vacant.
    ///
    /// Raises `PanicException` if the slot holds more than one member, which a
    /// scalar slot cannot have: reading a multi-member slot through this
    /// accessor is stale codegen, so regenerate the node's bindings and read it
    /// through `ObservationSlotSet.sources()`.
    fn source(&self) -> Option<PyObservedSource> {
        self.inner.source().map(PyObservedSource::from)
    }

    /// The sole pairing a `one` slot observes. The plan binds exactly one
    /// pairing to the slot and node startup re-checks its seed against the same
    /// rule, so a member always exists and there is no `None` to handle.
    ///
    /// Raises `PanicException` if the slot observes nothing, or more than one
    /// pairing: either means the generated code and the manifest disagree, so
    /// regenerate the node's bindings.
    fn sole_source(&self) -> PyObservedSource {
        PyObservedSource::from(self.inner.sole_source())
    }
}

/// Handle onto a multi-member observer slot's live observation state (a
/// `one_or_more` or `zero_or_more` slot), obtained via
/// `node_runner.observation_slot_set(link_id)`. The set is live in what it says
/// about each member: the daemon keeps every member's incarnation and liveness
/// current, and a member whose source is down stays in the list, at its
/// position. Its size does not move, so a `one_or_more` slot never reads empty
/// and only a `zero_or_more` slot does.
#[pyclass(name = "ObservationSlotSet")]
pub struct PyObservationSlotSet {
    pub(crate) inner: ObservationSlotSet,
}

#[pymethods]
impl PyObservationSlotSet {
    /// Every pairing this slot currently observes, in plan order: the order the
    /// launcher's array or the `--link` occurrences wrote, so member N here is
    /// the deployment's Nth entry for this slot. A `one_or_more` slot's list is
    /// never empty, so `[0]` is always valid there; a `zero_or_more` slot's list
    /// is empty wherever the plan bound no pairing at all.
    fn sources(&self) -> Vec<PyObservedSource> {
        self.inner
            .sources()
            .into_iter()
            .map(PyObservedSource::from)
            .collect()
    }

    fn __repr__(&self) -> String {
        let sources = self
            .sources()
            .iter()
            .map(PyObservedSource::__repr__)
            .collect::<Vec<_>>();
        format!("ObservationSlotSet(sources=[{}])", sources.join(", "))
    }
}

/// Stream of the observed sources' publishes on one topic, fanned in across the
/// slot's whole member set and vended by `node_runner.subscribe_observed(...)`.
/// Each `on_next_message()` yields a `(source, message)` tuple, or `None` when
/// the runtime is torn down. Delivery is a live stream, not a mailbox, and
/// follows each source instance's lifecycle independently of its peer
/// relationship.
#[pyclass(name = "ObservedSubscription")]
pub struct PyObservedSubscription {
    pub(crate) inner: Arc<Mutex<ObservedTopicSubscription>>,
}

#[pymethods]
impl PyObservedSubscription {
    /// Wait for and receive the next `(source, message)` from any currently
    /// observed source incarnation. Every cardinality fans in the same way and
    /// every message is tagged with the `ObservedSource` that published it, the
    /// same identity `ObservationSlotSet.sources()` enumerates, so a consumer
    /// routes on it even when several members share one instance. Returns
    /// `None` when the runtime is torn down. Messages buffered under a
    /// superseded source incarnation, or under a member the slot has since
    /// dropped, are dropped before they surface here.
    fn on_next_message<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut subscription = inner.lock().await;
            match subscription.on_next_message().await {
                Some((source, message)) => Ok(Some((
                    PyObservedSource::from(source),
                    PyTopicMessage::from(message),
                ))),
                None => Ok(None),
            }
        })
    }
}
