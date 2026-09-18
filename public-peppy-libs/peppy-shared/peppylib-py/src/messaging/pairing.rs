//! Python bindings for the pairing runtime surface: [`PyPeerInfo`] (identity
//! of a peer paired on a slot), [`PyPeerMember`] (a pair with its peer's
//! copy), [`PyPeerSlot`] and [`PyPeerSlotSet`] (observe a slot's peers),
//! [`PyPeerSubscription`] (receive what the peers publish) and
//! [`PyPeerPublisher`] (publish to each peer).

use super::target::PyProducerRef;
use super::topics::PyTopicMessage;
use peppylib::messaging::{PeerInfo, PeerMember};
use peppylib::runtime::{PeerPublisher, PeerSlot, PeerSlotSet, PeerSubscription};
use peppylib::types::Payload;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Identity of the peer paired on a pairing slot: the peer instance's full
/// `(core_node, instance_id)` wire address plus the link_id of the peer's own
/// complementary slot. Returned by `PeerSlot.paired()` / `wait_paired()` and
/// tagged onto every message a `PeerSubscription` yields. `frozen, eq, hash`
/// make it usable directly as a `dict` key, mirroring the Rust
/// `HashMap<PeerInfo, _>` idiom.
#[pyclass(name = "PeerInfo", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyPeerInfo {
    pub(crate) inner: PeerInfo,
}

#[pymethods]
impl PyPeerInfo {
    #[new]
    fn new(producer: PyProducerRef, peer_link_id: String) -> Self {
        Self {
            inner: PeerInfo {
                producer: producer.into_inner(),
                peer_link_id,
            },
        }
    }

    /// The peer instance's full wire address.
    #[getter]
    fn producer(&self) -> PyProducerRef {
        PyProducerRef::from(self.inner.producer.clone())
    }

    /// The link_id of the peer's complementary pairing slot.
    #[getter]
    fn peer_link_id(&self) -> &str {
        &self.inner.peer_link_id
    }

    fn __repr__(&self) -> String {
        format!(
            "PeerInfo(producer=ProducerRef({:?}, {:?}), peer_link_id={:?})",
            self.inner.producer.core_node, self.inner.producer.instance_id, self.inner.peer_link_id
        )
    }
}

impl From<PeerInfo> for PyPeerInfo {
    fn from(inner: PeerInfo) -> Self {
        Self { inner }
    }
}

/// One pair a slot holds: the peer's identity and the copy the peer's
/// instance belongs to (`None` for an instance run outside a copy). A node
/// holding pairs from several copies groups them by `copy`.
#[pyclass(name = "PeerMember", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyPeerMember {
    pub(crate) inner: PeerMember,
}

#[pymethods]
impl PyPeerMember {
    #[new]
    #[pyo3(signature = (info, copy=None))]
    fn new(info: &PyPeerInfo, copy: Option<String>) -> Self {
        Self {
            inner: PeerMember {
                info: info.inner.clone(),
                copy,
            },
        }
    }

    /// The peer's identity: its wire address and its complementary slot.
    #[getter]
    fn info(&self) -> PyPeerInfo {
        PyPeerInfo::from(self.inner.info.clone())
    }

    /// The copy the peer's instance belongs to, or `None`.
    #[getter]
    fn copy(&self) -> Option<&str> {
        self.inner.copy.as_deref()
    }

    fn __repr__(&self) -> String {
        let copy = match &self.inner.copy {
            Some(copy) => format!("{copy:?}"),
            None => "None".to_string(),
        };
        format!(
            "PeerMember(info={}, copy={})",
            PyPeerInfo::from(self.inner.info.clone()).__repr__(),
            copy
        )
    }
}

impl From<PeerMember> for PyPeerMember {
    fn from(inner: PeerMember) -> Self {
        Self { inner }
    }
}

/// Handle onto a scalar pairing slot's live state (a `one` or `zero_or_one`
/// slot), obtained via `node_runner.peer(link_id)`. `paired()` reads the
/// current peer (or `None` while unpaired); `wait_paired()` awaits one.
/// Multi-peer slots are read through [`PyPeerSlotSet`] instead.
#[pyclass(name = "PeerSlot")]
pub struct PyPeerSlot {
    pub(crate) inner: PeerSlot,
}

#[pymethods]
impl PyPeerSlot {
    /// The currently paired peer, or `None` while the slot is unpaired.
    ///
    /// Raises `PanicException` if the slot holds more than one pair, which a
    /// scalar slot cannot: reading a multi-peer slot through this accessor is
    /// stale codegen, so regenerate the node's bindings and read it through
    /// `PeerSlotSet.peers()`.
    fn paired(&self) -> Option<PyPeerInfo> {
        self.inner.paired().map(PyPeerInfo::from)
    }

    /// The currently paired peer with the copy it belongs to, or `None`
    /// while the slot is unpaired.
    fn paired_member(&self) -> Option<PyPeerMember> {
        self.inner.paired_member().map(PyPeerMember::from)
    }

    /// Wait until the slot is paired and return the peer's identity. Returns
    /// immediately when already paired.
    fn wait_paired<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let mut slot = self.inner.clone();
        crate::py_future::future_into_py(py, async move {
            slot.wait_paired()
                .await
                .map(PyPeerInfo::from)
                .map_err(super::to_py_err)
        })
    }
}

/// Handle onto a multi-peer pairing slot's live state (a `one_or_more` or
/// `zero_or_more` slot), obtained via `node_runner.peer_set(link_id)`. The
/// set is live: pairs join it when a peer's instance starts and leave it when
/// either side stops. A `one_or_more` slot's floor is a plan-time guarantee
/// that at least one peer is paired to it; the set still empties when every
/// peer stops.
#[pyclass(name = "PeerSlotSet")]
pub struct PyPeerSlotSet {
    pub(crate) inner: PeerSlotSet,
}

#[pymethods]
impl PyPeerSlotSet {
    /// Every pair the slot currently holds, in establishment order, each
    /// with the copy its peer belongs to.
    fn members(&self) -> Vec<PyPeerMember> {
        self.inner
            .members()
            .into_iter()
            .map(PyPeerMember::from)
            .collect()
    }

    /// The identity of every peer the slot currently holds, in
    /// establishment order.
    fn peers(&self) -> Vec<PyPeerInfo> {
        self.inner
            .peers()
            .into_iter()
            .map(PyPeerInfo::from)
            .collect()
    }

    fn __repr__(&self) -> String {
        let peers = self
            .peers()
            .iter()
            .map(PyPeerInfo::__repr__)
            .collect::<Vec<_>>();
        format!("PeerSlotSet(peers=[{}])", peers.join(", "))
    }
}

/// Publisher on one topic of a multi pairing slot, vended by
/// `node_runner.declare_peer_publisher(...)`. A pairing emission from a slot
/// holding several pairs names the peer it is for: `publish_to(peer, ...)`
/// names one, and raises for a peer the slot does not hold. A scalar slot
/// publishes through the `TopicPublisher`
/// `node_runner.declare_sole_peer_publisher(...)` vends.
#[pyclass(name = "PeerPublisher")]
pub struct PyPeerPublisher {
    pub(crate) inner: Arc<PeerPublisher>,
}

#[pymethods]
impl PyPeerPublisher {
    /// The identity of every peer the slot currently holds.
    fn peers(&self) -> Vec<PyPeerInfo> {
        self.inner
            .peers()
            .into_iter()
            .map(PyPeerInfo::from)
            .collect()
    }

    /// Publish to `peer`, one of the pairs the slot currently holds.
    fn publish_to<'py>(
        &self,
        py: Python<'py>,
        peer: &PyPeerInfo,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let publisher = Arc::clone(&self.inner);
        let peer = peer.inner.clone();
        crate::py_future::future_into_py(py, async move {
            publisher
                .publish_to(&peer, Payload::from(payload))
                .await
                .map_err(super::to_py_err)
        })
    }
}

/// Stream of the paired peers' publishes on one topic of a pairing slot,
/// vended by `node_runner.subscribe_peer(...)`. Each `on_next_message()`
/// yields a `(peer, message)` tuple, or `None` when the runtime is torn down.
/// Yields nothing while the slot holds no pair; delivery follows the slot's
/// live set.
#[pyclass(name = "PeerSubscription")]
pub struct PyPeerSubscription {
    pub(crate) inner: Arc<Mutex<PeerSubscription>>,
}

#[pymethods]
impl PyPeerSubscription {
    /// Wait for and receive the next `(peer, message)` from the currently
    /// paired peer: the same `PeerInfo` that `PeerSlot.paired()` returns, and
    /// the message it published. Returns `None` when the runtime is torn down.
    fn on_next_message<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut subscription = inner.lock().await;
            match subscription.on_next_message().await {
                Some((peer, message)) => Ok(Some((
                    PyPeerInfo::from(peer),
                    PyTopicMessage::from(message),
                ))),
                None => Ok(None),
            }
        })
    }
}
