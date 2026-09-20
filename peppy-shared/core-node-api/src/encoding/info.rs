//! Cap'n Proto encoding utilities for info messages.

use capnp::message::Builder;

use crate::info_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InfoRequest;

impl InfoRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<info_capnp::info_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<info_capnp::info_request::Reader>()?;
        Ok(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub apptainer_version: String,
    pub lima_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    pub uptime_secs: u64,
    pub core_node_name: String,
    pub core_node_instance_id: String,
    pub host_name: String,
    pub node_count: u32,
    pub git_version: String,
    pub container_info: ContainerInfo,
    pub messaging_port: u16,
}

impl InfoResponse {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        uptime_secs: u64,
        core_node_name: impl Into<String>,
        core_node_instance_id: impl Into<String>,
        host_name: impl Into<String>,
        node_count: u32,
        git_version: impl Into<String>,
        container_info: ContainerInfo,
        messaging_port: u16,
    ) -> Self {
        Self {
            uptime_secs,
            core_node_name: core_node_name.into(),
            core_node_instance_id: core_node_instance_id.into(),
            host_name: host_name.into(),
            node_count,
            git_version: git_version.into(),
            container_info,
            messaging_port,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<info_capnp::info_response::Builder>();
            response.set_uptime_secs(self.uptime_secs);
            response.set_core_node_name(&self.core_node_name);
            response.set_core_node_instance_id(&self.core_node_instance_id);
            response.set_host_name(&self.host_name);
            response.set_node_count(self.node_count);
            response.set_git_version(&self.git_version);
            response.set_messaging_port(self.messaging_port);
            let mut container = response.init_container_info();
            container.set_apptainer_version(&self.container_info.apptainer_version);
            container.set_lima_version(&self.container_info.lima_version);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<info_capnp::info_response::Reader>()?;
        let container = response.get_container_info()?;
        Ok(Self {
            uptime_secs: response.get_uptime_secs(),
            core_node_name: response.get_core_node_name()?.to_str()?.to_owned(),
            core_node_instance_id: response.get_core_node_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            node_count: response.get_node_count(),
            git_version: response.get_git_version()?.to_str()?.to_owned(),
            container_info: ContainerInfo {
                apptainer_version: container.get_apptainer_version()?.to_str()?.to_owned(),
                lima_version: container.get_lima_version()?.to_str()?.to_owned(),
            },
            messaging_port: response.get_messaging_port(),
        })
    }
}

impl crate::encoding::Wire for InfoRequest {
    type Root = crate::info_capnp::info_request::Owned;
}

impl crate::encoding::Wire for InfoResponse {
    type Root = crate::info_capnp::info_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_container_info() -> ContainerInfo {
        ContainerInfo {
            apptainer_version: "1.2.3".to_owned(),
            lima_version: "0.20.1".to_owned(),
        }
    }

    // Deliberately calls `::default()` on the unit struct to assert the Default
    // impl agrees with `new()`; that's what trips `default_constructed_unit_structs`.
    #[allow(clippy::default_constructed_unit_structs)]
    #[test]
    fn info_request_new_equals_default() {
        assert_eq!(InfoRequest::new(), InfoRequest);
        assert_eq!(InfoRequest::new(), InfoRequest::default());
    }

    #[test]
    fn info_request_roundtrip() {
        let original = InfoRequest::new();
        let encoded = original.encode().expect("encode");
        let decoded = InfoRequest::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn info_request_decode_rejects_malformed() {
        assert!(InfoRequest::decode(b"not a capnp message").is_err());
    }

    #[test]
    fn info_response_new_sets_fields() {
        let response = InfoResponse::new(
            3_600,
            "core-node",
            "instance-abc",
            "host-1",
            5,
            "v1.0.0",
            sample_container_info(),
            7777,
        );
        assert_eq!(response.uptime_secs, 3_600);
        assert_eq!(response.core_node_name, "core-node");
        assert_eq!(response.core_node_instance_id, "instance-abc");
        assert_eq!(response.host_name, "host-1");
        assert_eq!(response.node_count, 5);
        assert_eq!(response.git_version, "v1.0.0");
        assert_eq!(response.container_info, sample_container_info());
        assert_eq!(response.messaging_port, 7777);
    }

    #[test]
    fn info_response_roundtrip() {
        let original = InfoResponse::new(
            123_456,
            "core-node",
            "instance-abc",
            "host-1",
            5,
            "v1.0.0",
            sample_container_info(),
            7777,
        );
        let encoded = original.encode().expect("encode");
        let decoded = InfoResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn info_response_roundtrip_empty_strings_and_zero_counts() {
        let original = InfoResponse::new(
            0,
            "",
            "",
            "",
            0,
            "",
            ContainerInfo {
                apptainer_version: String::new(),
                lima_version: String::new(),
            },
            0,
        );
        let encoded = original.encode().expect("encode");
        let decoded = InfoResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn info_response_roundtrip_max_numeric_fields() {
        let original = InfoResponse::new(
            u64::MAX,
            "core-node",
            "instance-abc",
            "host-1",
            u32::MAX,
            "v1.0.0",
            sample_container_info(),
            u16::MAX,
        );
        let encoded = original.encode().expect("encode");
        let decoded = InfoResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded.uptime_secs, u64::MAX);
        assert_eq!(decoded.node_count, u32::MAX);
        assert_eq!(decoded.messaging_port, u16::MAX);
        assert_eq!(decoded, original);
    }

    #[test]
    fn info_response_roundtrip_preserves_container_info() {
        let container = ContainerInfo {
            apptainer_version: "9.9.9".to_owned(),
            lima_version: "8.8.8".to_owned(),
        };
        let original = InfoResponse::new(1, "n", "i", "h", 1, "g", container.clone(), 42);
        let encoded = original.encode().expect("encode");
        let decoded = InfoResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded.container_info, container);
    }

    #[test]
    fn info_response_decode_rejects_malformed() {
        assert!(InfoResponse::decode(b"not a capnp message").is_err());
    }
}
