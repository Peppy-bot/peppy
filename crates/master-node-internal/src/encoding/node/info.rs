//! Cap'n Proto encoding utilities for node info messages.

use bytes::Bytes;
use capnp::message::Builder;
use config::node::NodeConfig;
use std::path::PathBuf;

use crate::Result;
use crate::node_capnp;

use super::add::NodeSource;
use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfoRequest {
    pub source: NodeSource,
}

impl NodeInfoRequest {
    pub fn new(source: NodeSource) -> Self {
        Self { source }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_info_request::Builder>();
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
        use crate::node_capnp::node_info_request::source::Which;
        use gix_url::Url as GitUrl;

        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_info_request::Reader>()?;
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
}

#[derive(Debug, Clone)]
pub struct NodeInfoResponse {
    pub config: NodeConfig,
    pub is_in_node_stack: bool,
    pub instances_names: Vec<String>,
}

impl NodeInfoResponse {
    pub fn new(config: NodeConfig, is_in_node_stack: bool, instances_names: Vec<String>) -> Self {
        Self {
            config,
            is_in_node_stack,
            instances_names,
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_info_response::Builder>();
            let config_json5 = serde_json5::to_string(&self.config).map_err(|e| {
                crate::Error::Encoding(format!("failed to serialize config: {}", e))
            })?;
            response.set_config_json5(&config_json5);
            response.set_is_in_node_stack(self.is_in_node_stack);
            let mut instances = response.init_instances_names(self.instances_names.len() as u32);
            for (i, name) in self.instances_names.iter().enumerate() {
                instances.set(i as u32, name);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_info_response::Reader>()?;
        let config_json5 = response.get_config_json5()?.to_str()?;
        let config: NodeConfig = serde_json5::from_str(config_json5)
            .map_err(|e| crate::Error::Decoding(format!("failed to deserialize config: {}", e)))?;
        let is_in_node_stack = response.get_is_in_node_stack();
        let instances_names_reader = response.get_instances_names()?;
        let mut instances_names = Vec::with_capacity(instances_names_reader.len() as usize);
        for i in 0..instances_names_reader.len() {
            instances_names.push(instances_names_reader.get(i)?.to_str()?.to_owned());
        }
        Ok(Self {
            config,
            is_in_node_stack,
            instances_names,
        })
    }
}
