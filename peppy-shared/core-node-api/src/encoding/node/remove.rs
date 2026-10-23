use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, optional_text, read_text_list, write_text_list,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveRequest {
    pub node_name: String,
    pub tag: String,
    pub stop_instances: bool,
    /// The launch removing a node it added, which a daemon reserved for
    /// that launch admits; `None` for a request a user typed.
    pub launch_id: Option<String>,
}

impl NodeRemoveRequest {
    pub fn new(node_name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            node_name: node_name.into(),
            tag: tag.into(),
            stop_instances: false,
            launch_id: None,
        }
    }

    pub fn with_stop_instances(mut self, stop_instances: bool) -> Self {
        self.stop_instances = stop_instances;
        self
    }

    pub fn with_launch_id(mut self, launch_id: impl Into<String>) -> Self {
        self.launch_id = Some(launch_id.into());
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_remove_request::Builder>();
            request.set_node_name(&self.node_name);
            request.set_stop_instances(self.stop_instances);
            request.set_tag(&self.tag);
            if let Some(launch_id) = &self.launch_id {
                request.set_launch_id(launch_id);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_remove_request::Reader>()?;
        Ok(Self {
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            tag: request.get_tag()?.to_str()?.to_owned(),
            stop_instances: request.get_stop_instances(),
            launch_id: optional_text(request.get_launch_id()?.to_str()?),
        })
    }
}

/// The response of a node removal that succeeded. A failed removal is an
/// error reply, with a failure kind and a message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeRemoveResponse {
    /// The instances the removal stopped that did not exit within the
    /// cooperative shutdown window and were force-killed.
    pub force_killed: Vec<String>,
}

impl NodeRemoveResponse {
    /// A removal that force-killed the instances `force_killed`.
    pub fn new(force_killed: Vec<String>) -> Self {
        Self { force_killed }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_remove_response::Builder>();
            let force_killed = response.reborrow().init_force_killed(capnp_list_len(
                self.force_killed.len(),
                "NodeRemoveResponse.force_killed",
            )?);
            write_text_list(force_killed, &self.force_killed);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_remove_response::Reader>()?;
        Ok(Self {
            force_killed: read_text_list(response.get_force_killed()?)?,
        })
    }
}

impl crate::encoding::Wire for NodeRemoveRequest {
    type Root = crate::node_capnp::node_remove_request::Owned;
}

impl crate::encoding::Wire for NodeRemoveResponse {
    type Root = crate::node_capnp::node_remove_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_request_defaults_stop_instances_off() {
        let request = NodeRemoveRequest::new("my_node", "v1");
        assert!(!request.stop_instances);
        let payload = request.encode().expect("encode");
        let decoded = NodeRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.stop_instances);
        assert_eq!(decoded, request);
    }

    #[test]
    fn remove_request_round_trips_with_stop_instances_off() {
        let request = NodeRemoveRequest::new("my_node", "v1").with_stop_instances(false);
        assert!(!request.stop_instances);
        let payload = request.encode().expect("encode");
        let decoded = NodeRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.stop_instances);
        assert_eq!(decoded, request);
    }

    #[test]
    fn remove_request_round_trips_with_stop_instances_on() {
        let request = NodeRemoveRequest::new("my_node", "v1").with_stop_instances(true);
        assert!(request.stop_instances);
        let payload = request.encode().expect("encode");
        let decoded = NodeRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert!(decoded.stop_instances);
        assert_eq!(decoded.node_name, "my_node");
        assert_eq!(decoded.tag, "v1");
        assert_eq!(decoded, request);
    }

    /// The launch id is what a reserved daemon admits the removal on, so it
    /// has to survive the wire and be readable through the trait the daemon
    /// gates on.
    #[test]
    fn remove_request_round_trips_a_launch_id() {
        use crate::LaunchScoped;

        let request = NodeRemoveRequest::new("my_node", "v1").with_launch_id("launch-abc123");
        let decoded =
            NodeRemoveRequest::decode(request.encode().expect("encode").as_ref()).expect("decode");
        assert_eq!(decoded.launch_id.as_deref(), Some("launch-abc123"));
        assert_eq!(decoded.launch_id(), Some("launch-abc123"));
        assert_eq!(decoded, request);
        assert_eq!(NodeRemoveRequest::new("my_node", "v1").launch_id(), None);
    }

    #[test]
    fn remove_request_decode_rejects_malformed_bytes() {
        NodeRemoveRequest::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }

    #[test]
    fn remove_response_round_trips_a_graceful_removal() {
        let response = NodeRemoveResponse::default();
        let payload = response.encode().expect("encode");
        let decoded = NodeRemoveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn remove_response_round_trips_the_force_killed_instances() {
        let response = NodeRemoveResponse::new(vec!["arm_inst".to_owned(), "cam_inst".to_owned()]);
        let payload = response.encode().expect("encode");
        let decoded = NodeRemoveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.force_killed, ["arm_inst", "cam_inst"]);
        assert_eq!(decoded, response);
    }

    #[test]
    fn remove_response_decode_rejects_malformed_bytes() {
        NodeRemoveResponse::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }
}
