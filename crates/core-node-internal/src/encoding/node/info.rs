//! Cap'n Proto encoding utilities for node info messages.

use std::time::Duration;

use capnp::message::Builder;
use config::node::NodeConfig;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::add::NodeSource;
use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfoRequest {
    pub source: NodeSource,
}

impl NodeInfoRequest {
    pub fn new(source: NodeSource) -> Self {
        Self { source }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_info_request::Builder>();
            let mut source = request.reborrow().init_source();
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
                NodeSource::Http { url, sha256 } => {
                    source.set_http(url.as_str());
                    if let Some(digest) = NodeSource::normalize_http_sha256(sha256.as_deref()) {
                        request.reborrow().set_http_sha256(&digest);
                    }
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::node_capnp::node_info_request::source::Which;

        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_info_request::Reader>()?;
        let source = match request.get_source().which()? {
            Which::Fs(fs) => NodeSource::decode_fs(fs?.to_str()?)?,
            Which::Git(git) => {
                let git = git?;
                NodeSource::decode_git(
                    git.get_repo_url()?.to_str()?,
                    git.get_repo_path()?.to_str()?,
                    git.get_repo_ref()?.to_str()?,
                )?
            }
            Which::Http(http) => NodeSource::decode_http(
                http?.to_str()?,
                Some(request.get_http_sha256()?.to_str()?),
            )?,
        };

        Ok(Self { source })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeInfoResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_INFO,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeInfoResponse::decode(response.payload().as_ref())
    }
}

#[derive(Debug, Clone)]
pub struct NodeInfoResponse {
    pub config: NodeConfig,
    pub is_in_node_stack: bool,
    pub instances_names: Vec<String>,
    /// SHA256 of the entire NodeConfig file taken from NodeSource
    pub config_integrity: String,
    /// Name of the variant applied, if any.
    pub variant_name: Option<String>,
    /// Non-fatal issues encountered during resolution (e.g. unknown variant).
    pub issues: Vec<String>,
}

impl NodeInfoResponse {
    pub fn new(
        config: NodeConfig,
        is_in_node_stack: bool,
        instances_names: Vec<String>,
        config_integrity: String,
        variant_name: Option<String>,
        issues: Vec<String>,
    ) -> Self {
        Self {
            config,
            is_in_node_stack,
            instances_names,
            config_integrity,
            variant_name,
            issues,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_info_response::Builder>();
            let config_json5 = serde_json5::to_string(&self.config).map_err(|e| {
                crate::Error::Encoding(format!("failed to serialize config: {}", e))
            })?;
            response.set_config_json5(&config_json5);
            response.set_is_in_node_stack(self.is_in_node_stack);
            let mut instances = response
                .reborrow()
                .init_instances_names(self.instances_names.len() as u32);
            for (i, name) in self.instances_names.iter().enumerate() {
                instances.set(i as u32, name);
            }
            response.set_config_sha256(&self.config_integrity);
            response.set_variant_name(self.variant_name.as_deref().unwrap_or(""));
            let mut issues_builder = response.reborrow().init_issues(self.issues.len() as u32);
            for (i, issue) in self.issues.iter().enumerate() {
                issues_builder.set(i as u32, issue);
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
        let config_integrity = response.get_config_sha256()?.to_str()?.to_owned();
        let variant_name = optional_text(response.get_variant_name()?.to_str()?);
        let issues_reader = response.get_issues()?;
        let mut issues = Vec::with_capacity(issues_reader.len() as usize);
        for i in 0..issues_reader.len() {
            issues.push(issues_reader.get(i)?.to_str()?.to_owned());
        }
        Ok(Self {
            config,
            is_in_node_stack,
            instances_names,
            config_integrity,
            variant_name,
            issues,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_info_request_http_source_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/node.tar.zst").unwrap();
        let sha256 = "c".repeat(64);

        let encoded = NodeInfoRequest::new(NodeSource::Http {
            url: url.clone(),
            sha256: Some(sha256.clone()),
        })
        .encode()
        .expect("encoding should succeed");
        let decoded = NodeInfoRequest::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.source,
            NodeSource::Http {
                url,
                sha256: Some(sha256)
            }
        );
    }
}
