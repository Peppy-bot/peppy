//! Cap'n Proto encoding utilities for ready messages.

use crate::health_capnp;

super::capnp_empty_message!(
    NodeReadyRequest,
    health_capnp::node_ready_request::Builder<'_>,
    health_capnp::node_ready_request::Reader<'_>
);

super::capnp_empty_message!(
    NodeReadyResponse,
    health_capnp::node_ready_response::Builder<'_>,
    health_capnp::node_ready_response::Reader<'_>
);
