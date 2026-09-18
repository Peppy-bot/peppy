//! Cap'n Proto encoding utilities for health messages.

use crate::health_capnp;

super::capnp_empty_message!(
    NodeHealthRequest,
    health_capnp::node_health_request::Builder,
    health_capnp::node_health_request::Reader
);

super::capnp_empty_message!(
    NodeHealthResponse,
    health_capnp::node_health_response::Builder,
    health_capnp::node_health_response::Reader
);
