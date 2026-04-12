use capnp::message::Builder;
use peppylib::types::Payload;

use crate::Result;
use crate::repo_capnp;

use crate::encoding::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSource {
    Git {
        repo_url: String,
        repo_ref: Option<String>,
    },
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAddRequest {
    pub source: RepoSource,
}

impl RepoAddRequest {
    pub fn new_git(repo_url: impl Into<String>, repo_ref: Option<String>) -> Self {
        Self {
            source: RepoSource::Git {
                repo_url: repo_url.into(),
                repo_ref,
            },
        }
    }

    pub fn new_url(url: impl Into<String>) -> Self {
        Self {
            source: RepoSource::Url(url.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<repo_capnp::repo_add_request::Builder>();
            let mut source = request.reborrow().init_source();
            match &self.source {
                RepoSource::Git {
                    repo_url,
                    repo_ref,
                } => {
                    let mut git = source.init_git();
                    git.set_repo_url(repo_url);
                    git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                }
                RepoSource::Url(url) => {
                    source.set_url(url);
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::repo_capnp::repo_add_request::source::Which;

        let reader = decode_message(data)?;
        let request = reader.get_root::<repo_capnp::repo_add_request::Reader>()?;
        let source = match request.get_source().which()? {
            Which::Git(git) => {
                let git = git?;
                let repo_url = git.get_repo_url()?.to_str()?.to_owned();
                let repo_ref_str = git.get_repo_ref()?.to_str()?.to_owned();
                let repo_ref = if repo_ref_str.is_empty() {
                    None
                } else {
                    Some(repo_ref_str)
                };
                RepoSource::Git {
                    repo_url,
                    repo_ref,
                }
            }
            Which::Url(url) => RepoSource::Url(url?.to_str()?.to_owned()),
        };
        Ok(Self { source })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAddResponse {
    pub success: bool,
    pub error_message: String,
}

impl RepoAddResponse {
    pub fn success() -> Self {
        Self {
            success: true,
            error_message: String::new(),
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_add_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_add_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}
