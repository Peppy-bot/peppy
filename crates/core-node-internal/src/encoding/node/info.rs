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
use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfoRequest {
    pub source: NodeSource,
    pub variant: Option<NodeSource>,
}

impl NodeInfoRequest {
    pub fn new(source: NodeSource) -> Self {
        Self {
            source,
            variant: None,
        }
    }

    pub fn with_variant(mut self, variant: NodeSource) -> Self {
        self.variant = Some(variant);
        self
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

            if let Some(ref variant) = self.variant {
                let mut variant_builder = request.reborrow().init_variant();
                let mut variant_source = variant_builder.reborrow().init_source();
                match variant {
                    NodeSource::Fs(name) => {
                        variant_source.set_fs(name.to_string_lossy().as_ref());
                    }
                    NodeSource::Git {
                        repo_url,
                        repo_path,
                        repo_ref,
                    } => {
                        let mut git = variant_source.init_git();
                        git.set_repo_url(repo_url.to_bstring().to_string());
                        git.set_repo_path(repo_path);
                        git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                    }
                    NodeSource::Http { url, sha256 } => {
                        variant_source.set_http(url.as_str());
                        if let Some(digest) = NodeSource::normalize_http_sha256(sha256.as_deref()) {
                            variant_builder.set_http_sha256(&digest);
                        }
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

        let variant = if request.has_variant() {
            use crate::node_capnp::node_add_variant_source::source::Which;
            let variant_reader = request.get_variant()?;
            Some(match variant_reader.get_source().which()? {
                Which::Fs(fs) => {
                    let name = fs?.to_str()?;
                    if name.is_empty() {
                        return Err(crate::Error::Decoding(
                            "empty fs path in variant source".into(),
                        ));
                    }
                    NodeSource::decode_fs(name)?
                }
                Which::Git(git) => {
                    let git = git?;
                    let repo_url_str = git.get_repo_url()?.to_str()?;
                    if repo_url_str.is_empty() {
                        return Err(crate::Error::Decoding(
                            "empty git repo URL in variant source".into(),
                        ));
                    }
                    NodeSource::decode_git(
                        repo_url_str,
                        git.get_repo_path()?.to_str()?,
                        git.get_repo_ref()?.to_str()?,
                    )?
                }
                Which::Http(http) => {
                    let url_str = http?.to_str()?;
                    if url_str.is_empty() {
                        return Err(crate::Error::Decoding(
                            "empty http URL in variant source".into(),
                        ));
                    }
                    NodeSource::decode_http(
                        url_str,
                        Some(variant_reader.get_http_sha256()?.to_str()?),
                    )?
                }
            })
        } else {
            None
        };

        Ok(Self { source, variant })
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
        let variant_name = response.get_variant_name()?.to_str()?.to_owned();
        let variant_name = if variant_name.is_empty() {
            None
        } else {
            Some(variant_name)
        };
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

    /// Helper: build a NodeInfoRequest message with a valid Fs source and a
    /// variant whose source union is set by the caller-supplied closure, then
    /// return the encoded bytes.
    fn build_malformed_variant(
        set_variant: impl FnOnce(crate::node_capnp::node_add_variant_source::Builder),
    ) -> Payload {
        let mut builder = capnp::message::Builder::new_default();
        {
            let mut request = builder.init_root::<crate::node_capnp::node_info_request::Builder>();
            request.reborrow().init_source().set_fs("/root");
            let variant_builder = request.reborrow().init_variant();
            set_variant(variant_builder);
        }
        super::encode_message(&builder).expect("encoding should succeed")
    }

    #[test]
    fn decode_rejects_variant_with_empty_fs_path() {
        let data = build_malformed_variant(|mut v| {
            v.reborrow().init_source().set_fs("");
        });
        let err = NodeInfoRequest::decode(&data).expect_err("should reject empty fs variant");
        assert!(
            err.to_string().contains("empty fs path in variant source"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_rejects_variant_with_empty_git_repo_url() {
        let data = build_malformed_variant(|mut v| {
            let mut git = v.reborrow().init_source().init_git();
            git.set_repo_url("");
            git.set_repo_path("some/path");
            git.set_repo_ref("main");
        });
        let err =
            NodeInfoRequest::decode(&data).expect_err("should reject empty git repo URL variant");
        assert!(
            err.to_string()
                .contains("empty git repo URL in variant source"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_rejects_variant_with_empty_http_url() {
        let data = build_malformed_variant(|mut v| {
            v.reborrow().init_source().set_http("");
        });
        let err = NodeInfoRequest::decode(&data).expect_err("should reject empty http URL variant");
        assert!(
            err.to_string().contains("empty http URL in variant source"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn node_info_request_http_variant_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/variant.tar.zst").unwrap();
        let sha256 = "d".repeat(64);

        let encoded = NodeInfoRequest::new(NodeSource::Fs("/root".into()))
            .with_variant(NodeSource::Http {
                url: url.clone(),
                sha256: Some(sha256.clone()),
            })
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoRequest::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.variant,
            Some(NodeSource::Http {
                url,
                sha256: Some(sha256)
            })
        );
    }
}
