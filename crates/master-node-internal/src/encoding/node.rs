//! Cap'n Proto encoding utilities for node messages.

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use config::peppy_config::BuildSystem;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::node_capnp;

use super::{decode_message, encode_message};

// ============================================================================
// Node List
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListRequest;

impl NodeListRequest {
    pub fn new() -> Self {
        Self
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<node_capnp::node_list_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader.get_root::<node_capnp::node_list_request::Reader>()?;
        Ok(Self)
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeListResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_list",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeListResponse::decode(&response.payload().to_bytes())
    }
}

impl Default for NodeListRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListResponse {
    pub dot_graph: String,
}

impl NodeListResponse {
    pub fn new(dot_graph: impl Into<String>) -> Self {
        Self {
            dot_graph: dot_graph.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_list_response::Builder>();
            response.set_dot_graph(&self.dot_graph);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_list_response::Reader>()?;
        Ok(Self {
            dot_graph: response.get_dot_graph()?.to_str()?.to_owned(),
        })
    }
}

// ============================================================================
// Node Add
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddRequest {
    pub peppy_json5: String,
    pub from_dir: PathBuf,
    pub instance_id: Option<String>,
    pub run_immediately: bool,
}

impl NodeAddRequest {
    pub fn new(peppy_json5: impl Into<String>, from_dir: impl Into<PathBuf>) -> Self {
        Self {
            peppy_json5: peppy_json5.into(),
            from_dir: from_dir.into(),
            instance_id: None,
            run_immediately: false,
        }
    }

    pub fn with_instance_id(mut self, instance_id: impl Into<String>) -> Self {
        self.instance_id = Some(instance_id.into());
        self
    }

    pub fn with_run_immediately(mut self, run_immediately: bool) -> Self {
        self.run_immediately = run_immediately;
        self
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_add_request::Builder>();
            request.set_peppy_json5(&self.peppy_json5);
            request.set_from_dir(&self.from_dir.to_string_lossy());
            if let Some(ref instance_id) = self.instance_id {
                request.set_instance_id(instance_id);
            }
            request.set_run_immediately(self.run_immediately);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_add_request::Reader>()?;
        let instance_id_str = request.get_instance_id()?.to_str()?;
        let instance_id = if instance_id_str.is_empty() {
            None
        } else {
            Some(instance_id_str.to_owned())
        };
        Ok(Self {
            peppy_json5: request.get_peppy_json5()?.to_str()?.to_owned(),
            from_dir: PathBuf::from(request.get_from_dir()?.to_str()?),
            instance_id,
            run_immediately: request.get_run_immediately(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeAddResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_add",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeAddResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddResponse {
    pub success: bool,
    pub is_running: bool,
    pub node_instance_id: String,
    pub error_message: Option<String>,
}

impl NodeAddResponse {
    pub fn new(
        success: bool,
        is_running: bool,
        node_instance_id: impl Into<String>,
        error_message: Option<String>,
    ) -> Self {
        Self {
            success,
            is_running,
            node_instance_id: node_instance_id.into(),
            error_message,
        }
    }

    pub fn success(node_instance_id: impl Into<String>, is_running: bool) -> Self {
        Self::new(true, is_running, node_instance_id, None)
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, false, "", Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_add_response::Builder>();
            response.set_success(self.success);
            response.set_is_running(self.is_running);
            response.set_node_instance_id(&self.node_instance_id);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_add_response::Reader>()?;
        let error_message_str = response.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        Ok(Self {
            success: response.get_success(),
            is_running: response.get_is_running(),
            node_instance_id: response.get_node_instance_id()?.to_str()?.to_owned(),
            error_message,
        })
    }
}

// ============================================================================
// Node Init
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitRequest {
    pub node_root_dir: PathBuf,
    pub build_system: BuildSystem,
    pub node_name: String,
}

impl NodeInitRequest {
    pub fn new(node_root_dir: impl Into<PathBuf>, node_name: impl Into<String>) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            build_system: BuildSystem::Cargo,
            node_name: node_name.into(),
        }
    }

    pub fn with_build_system(mut self, build_system: BuildSystem) -> Self {
        self.build_system = build_system;
        self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_init_request::Builder>();
            request.set_node_root_dir(&self.node_root_dir.to_string_lossy());
            request.set_build_system(&self.build_system.to_string());
            request.set_node_name(&self.node_name);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_init_request::Reader>()?;
        let build_system_str = request.get_build_system()?.to_str()?;
        let build_system = build_system_str.parse()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            build_system,
            node_name: request.get_node_name()?.to_str()?.to_owned(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeInitResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_init",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeInitResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeInitResponse {
    pub fn new(success: bool, error_message: impl Into<String>) -> Self {
        Self {
            success,
            error_message: error_message.into(),
        }
    }

    pub fn success() -> Self {
        Self::new(true, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, error_message)
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_init_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_init_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

// ============================================================================
// Node Sync
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest {
    pub node_root_dir: PathBuf,
    pub build_system: BuildSystem,
}

impl NodeSyncRequest {
    pub fn new(node_root_dir: impl Into<PathBuf>) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            build_system: BuildSystem::Cargo,
        }
    }

    pub fn with_build_system(mut self, build_system: BuildSystem) -> Self {
        self.build_system = build_system;
        self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_sync_request::Builder>();
            request.set_node_root_dir(&self.node_root_dir.to_string_lossy());
            request.set_language(&self.build_system.to_string());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_sync_request::Reader>()?;
        let build_system_str = request.get_language()?.to_str()?;
        let build_system = build_system_str.parse()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            build_system,
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeSyncResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_sync",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeSyncResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeSyncResponse {
    pub fn new(success: bool, error_message: impl Into<String>) -> Self {
        Self {
            success,
            error_message: error_message.into(),
        }
    }

    pub fn success() -> Self {
        Self::new(true, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, error_message)
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_sync_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_sync_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

// ============================================================================
// Node Run
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunRequest {
    pub instance_id: String,
}

impl NodeRunRequest {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
        }
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_run_request::Builder>();
            request.set_instance_id(&self.instance_id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_run_request::Reader>()?;
        Ok(Self {
            instance_id: request.get_instance_id()?.to_str()?.to_owned(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeRunResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_run",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeRunResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeRunResponse {
    pub fn new(success: bool, error_message: Option<String>) -> Self {
        Self {
            success,
            error_message,
        }
    }

    pub fn success() -> Self {
        Self::new(true, None)
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_run_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_run_response::Reader>()?;
        let error_message_str = response.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        Ok(Self {
            success: response.get_success(),
            error_message,
        })
    }
}
