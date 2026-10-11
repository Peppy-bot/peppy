pub mod transport;

use std::time::Duration;

/// How long a wrapper waits for its core node's reply when the caller gives
/// no timeout.
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Polls a core-node service on `node_runner`'s bound core node, as that
/// node: the routing every high-level wrapper (`info`, `clock`, `datastore`,
/// `stack`) shares. `None` waits [`DEFAULT_RESPONSE_TIMEOUT`].
pub(crate) async fn poll_bound_core_node<R: core_node_api::ServiceRequest>(
    node_runner: &crate::runtime::NodeRunner,
    request: &R,
    response_timeout: Option<Duration>,
) -> crate::error::Result<R::Response> {
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();
    transport::poll(
        request,
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        response_timeout.unwrap_or(DEFAULT_RESPONSE_TIMEOUT),
    )
    .await
}

/// Subscribes to a periodic core-node-published topic (e.g. `clock`,
/// `daemon_heartbeat`) on `node_runner`'s bound core node, keyed the same way
/// the daemon publishes it: the target's name segment says which daemon's
/// stream this matches (a daemon's node name IS its core_node name), and the
/// reserved default `link_id` says it is that daemon's own publishes, while
/// its per-boot `(core_node, instance_id)` pair stays wildcarded. A clock
/// domain hosted on the same machine rides this same topic and target under
/// the domain's `link_id`, which the pinned segment leaves out. `SensorData`
/// QoS: for these streams a slow subscriber should get newer messages dropped
/// rather than back-pressure the publisher.
pub(crate) async fn subscribe_core_topic(
    node_runner: &crate::runtime::NodeRunner,
    topic: &str,
) -> crate::error::Result<crate::messaging::Subscription> {
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();
    crate::messaging::TopicMessenger::subscribe_target_scoped_with_link_id(
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        crate::messaging::SenderTarget::node(core_node, core_node_api::names::CORE_NODE_TAG)?,
        Some(pmi::DEFAULT_LINK_ID),
        topic,
        config::node::QoSProfile::SensorData,
    )
    .await
}
