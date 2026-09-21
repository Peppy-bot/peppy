//! Cap'n Proto codec for the framework `node_endpoints` service: the sockets
//! a node bound during setup, one per label its manifest declares. See
//! `schemas/endpoints.capnp` for the wire contract.

use std::net::{IpAddr, SocketAddr};

use crate::endpoints_capnp;
use crate::error::{Error, Result};
use crate::runtime::{AnnouncedEndpoint, EndpointBinding};
use crate::types::Payload;

super::capnp_empty_message!(
    NodeEndpointsRequest,
    endpoints_capnp::node_endpoints_request::Builder,
    endpoints_capnp::node_endpoints_request::Reader
);

/// The sealed announced set of an instance, in label order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeEndpointsResponse {
    pub endpoints: Vec<AnnouncedEndpoint>,
}

impl NodeEndpointsResponse {
    pub fn new(endpoints: Vec<AnnouncedEndpoint>) -> Self {
        Self { endpoints }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = ::capnp::message::Builder::new_default();
        {
            let root = builder.init_root::<endpoints_capnp::node_endpoints_response::Builder>();
            let mut wire_endpoints = root.init_endpoints(self.endpoints.len() as u32);
            for (idx, endpoint) in self.endpoints.iter().enumerate() {
                let mut wire = wire_endpoints.reborrow().get(idx as u32);
                wire.set_label(&endpoint.label);
                wire.set_scheme(&endpoint.binding.scheme);
                wire.set_host(&endpoint.binding.address.ip().to_string());
                wire.set_port(endpoint.binding.address.port());
                wire.set_path(&endpoint.binding.path);
            }
        }
        super::encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = super::decode_message(data)?;
        let root = reader
            .get_root::<endpoints_capnp::node_endpoints_response::Reader>()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let wire_endpoints = root
            .get_endpoints()
            .map_err(|e| Error::Deserialization(e.to_string()))?;
        let mut endpoints = Vec::with_capacity(wire_endpoints.len() as usize);
        for idx in 0..wire_endpoints.len() {
            let wire = wire_endpoints.get(idx);
            let label = super::read_text(wire.get_label(), "node_endpoints", "label")?;
            let host = super::read_text(wire.get_host(), "node_endpoints", "host")?;
            let ip: IpAddr = host.parse().map_err(|e| {
                Error::Deserialization(format!(
                    "node_endpoints field `host` for `{label}` is not an IP literal (`{host}`): {e}"
                ))
            })?;
            endpoints.push(AnnouncedEndpoint {
                label,
                binding: EndpointBinding {
                    scheme: super::read_text(wire.get_scheme(), "node_endpoints", "scheme")?,
                    address: SocketAddr::new(ip, wire.get_port()),
                    path: super::read_text(wire.get_path(), "node_endpoints", "path")?,
                },
            });
        }
        Ok(Self { endpoints })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announced(label: &str, scheme: &str, address: &str, path: &str) -> AnnouncedEndpoint {
        AnnouncedEndpoint {
            label: label.to_string(),
            binding: EndpointBinding {
                scheme: scheme.to_string(),
                address: address.parse().expect("a socket address"),
                path: path.to_string(),
            },
        }
    }

    #[test]
    fn a_response_round_trips_every_host_shape() {
        let response = NodeEndpointsResponse::new(vec![
            announced("any_v4", "http", "0.0.0.0:8765", ""),
            announced("any_v6", "https", "[::]:8080", "/"),
            announced("loopback", "http", "127.0.0.1:8900", "/camera/v1/mcp"),
            announced("v6_literal", "https", "[2001:db8::7]:4443", "/task"),
        ]);
        let decoded = NodeEndpointsResponse::decode(&response.encode().unwrap().into_inner())
            .expect("decodes");
        assert_eq!(decoded, response);
    }

    #[test]
    fn an_empty_response_round_trips() {
        let response = NodeEndpointsResponse::new(Vec::new());
        let decoded = NodeEndpointsResponse::decode(&response.encode().unwrap().into_inner())
            .expect("decodes");
        assert!(decoded.endpoints.is_empty());
    }

    #[test]
    fn the_request_round_trips() {
        let payload = NodeEndpointsRequest::new().encode().expect("encodes");
        NodeEndpointsRequest::decode(&payload.into_inner()).expect("decodes");
    }
}
