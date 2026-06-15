use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackListRequest {
    with_dot_graph: bool,
}

impl StackListRequest {
    pub fn new(with_dot_graph: bool) -> Self {
        Self { with_dot_graph }
    }

    pub fn with_dot_graph(&self) -> bool {
        self.with_dot_graph
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_list_request::Builder>();
            request.set_with_dot_graph(self.with_dot_graph);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_list_request::Reader>()?;
        Ok(Self {
            with_dot_graph: request.get_with_dot_graph(),
        })
    }
}

impl Default for StackListRequest {
    fn default() -> Self {
        Self::new(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackListResponse {
    pub dot_graph: Option<String>,
    pub graph_json: String,
}

impl StackListResponse {
    pub fn new(dot_graph: Option<String>, graph_json: impl Into<String>) -> Self {
        Self {
            dot_graph,
            graph_json: graph_json.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_list_response::Builder>();
            if let Some(ref dot_graph) = self.dot_graph {
                response.set_dot_graph(dot_graph);
            }
            response.set_graph_json(&self.graph_json);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_list_response::Reader>()?;
        Ok(Self {
            dot_graph: optional_text(response.get_dot_graph()?.to_str()?),
            graph_json: response.get_graph_json()?.to_str()?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_default_disables_dot_graph() {
        let request = StackListRequest::default();
        assert!(!request.with_dot_graph());
    }

    #[test]
    fn request_round_trips_without_dot_graph() {
        let request = StackListRequest::new(false);
        assert!(!request.with_dot_graph());
        let payload = request.encode().expect("encode");
        let decoded = StackListRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert!(!decoded.with_dot_graph());
    }

    #[test]
    fn request_round_trips_with_dot_graph() {
        let request = StackListRequest::new(true);
        assert!(request.with_dot_graph());
        let payload = request.encode().expect("encode");
        let decoded = StackListRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert!(decoded.with_dot_graph());
    }

    #[test]
    fn request_decode_rejects_malformed() {
        assert!(StackListRequest::decode(b"not capnp").is_err());
    }

    #[test]
    fn response_round_trips_with_dot_graph_and_json() {
        let response = StackListResponse::new(
            Some("digraph { a -> b }".to_string()),
            r#"{"nodes":["a","b"],"edges":[["a","b"]]}"#,
        );
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert_eq!(decoded.dot_graph.as_deref(), Some("digraph { a -> b }"));
    }

    #[test]
    fn response_round_trips_without_dot_graph() {
        // `None` dot_graph leaves the field unset; the empty wire string decodes
        // back to `None` via `optional_text`.
        let response = StackListResponse::new(None, r#"{"nodes":[]}"#);
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert_eq!(decoded.dot_graph, None);
        assert_eq!(decoded.graph_json, r#"{"nodes":[]}"#);
    }

    #[test]
    fn response_round_trips_empty_graph_json() {
        let response = StackListResponse::new(None, "");
        let payload = response.encode().expect("encode");
        let decoded = StackListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.graph_json.is_empty());
    }

    #[test]
    fn response_decode_rejects_malformed() {
        assert!(StackListResponse::decode(b"not capnp").is_err());
    }
}
