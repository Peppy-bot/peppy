use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use config::node::InterfaceKind;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::add::NodeSource;
use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIntegrityRequest {
    pub source: NodeSource,
}

impl NodeIntegrityRequest {
    pub fn new(source: NodeSource) -> Self {
        Self { source }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let request = builder.init_root::<node_capnp::node_integrity_request::Builder>();
            let mut source = request.init_source();
            match &self.source {
                NodeSource::Fs(path) => {
                    source.set_fs(path.to_string_lossy().as_ref());
                }
                NodeSource::Git {
                    repo_url,
                    repo_path,
                    repo_ref,
                } => {
                    let mut git = source.init_git();
                    git.set_repo_url(repo_url.to_bstring().to_string());
                    git.set_repo_path(repo_path);
                    git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                }
                NodeSource::Http { url } => {
                    source.set_http(url.as_str());
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::node_capnp::node_integrity_request::source::Which;
        use gix_url::Url as GitUrl;

        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_integrity_request::Reader>()?;
        let source = match request.get_source().which()? {
            Which::Fs(fs) => NodeSource::Fs(PathBuf::from(fs?.to_str()?)),
            Which::Git(git) => {
                let git = git?;
                let repo_url_str = git.get_repo_url()?.to_str()?;
                let repo_url = GitUrl::try_from(repo_url_str)
                    .map_err(|e| crate::Error::Decoding(format!("invalid git URL: {}", e)))?;
                let repo_path = git.get_repo_path()?.to_str()?.to_owned();
                let repo_ref = git.get_repo_ref()?.to_str()?.trim().to_owned();
                let repo_ref = if repo_ref.is_empty() {
                    None
                } else {
                    Some(repo_ref)
                };
                NodeSource::Git {
                    repo_url,
                    repo_path,
                    repo_ref,
                }
            }
            Which::Http(http) => {
                let url_str = http?.to_str()?;
                let url = url::Url::parse(url_str)
                    .map_err(|e| crate::Error::Decoding(format!("invalid HTTP URL: {}", e)))?;
                NodeSource::Http { url }
            }
        };
        Ok(Self { source })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeIntegrityResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_INTEGRITY,
            Some(target_master_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeIntegrityResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceIntegrity {
    pub name: String,
    pub sha256: String,
    pub interface_kind: InterfaceKind,
}

#[derive(Debug, Clone)]
pub struct NodeIntegrityResponse {
    /// Sha256 for each exposed interface of the NodeSource
    pub interfaces_integrity: Vec<InterfaceIntegrity>,
    /// Sha256 of the entire NodeConfig file taken from NodeSource
    pub config_integrity: String,
}

impl NodeIntegrityResponse {
    pub fn new(interfaces_integrity: Vec<InterfaceIntegrity>, config_integrity: String) -> Self {
        Self {
            interfaces_integrity,
            config_integrity,
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_integrity_response::Builder>();
            let mut entries = response
                .reborrow()
                .init_interfaces_integrity(self.interfaces_integrity.len() as u32);
            for (i, iface) in self.interfaces_integrity.iter().enumerate() {
                let mut entry = entries.reborrow().get(i as u32);
                entry.set_name(&iface.name);
                entry.set_sha256(&iface.sha256);
                entry.set_interface_kind(&iface.interface_kind.to_string());
            }
            response.set_config_sha256(&self.config_integrity);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_integrity_response::Reader>()?;
        let entries = response.get_interfaces_integrity()?;
        let mut interfaces_integrity = Vec::with_capacity(entries.len() as usize);
        for i in 0..entries.len() {
            let entry = entries.get(i);
            let interface_kind = entry
                .get_interface_kind()?
                .to_str()?
                .parse::<InterfaceKind>()
                .map_err(crate::Error::Decoding)?;
            interfaces_integrity.push(InterfaceIntegrity {
                name: entry.get_name()?.to_str()?.to_owned(),
                sha256: entry.get_sha256()?.to_str()?.to_owned(),
                interface_kind,
            });
        }
        let config_integrity = response.get_config_sha256()?.to_str()?.to_owned();
        Ok(Self {
            interfaces_integrity,
            config_integrity,
        })
    }
}
