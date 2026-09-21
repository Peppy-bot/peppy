//! Cap'n Proto encoding utilities for ready messages.

use crate::health_capnp;

super::capnp_empty_message!(
    NodeReadyRequest,
    health_capnp::node_ready_request::Builder,
    health_capnp::node_ready_request::Reader
);

super::capnp_empty_message!(
    NodeReadyResponse,
    health_capnp::node_ready_response::Builder,
    health_capnp::node_ready_response::Reader
);
