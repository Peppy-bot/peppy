use std::path::PathBuf;

use capnp::message::Builder;

use crate::repo_capnp;
use crate::{Payload, Result};

use crate::encoding::RepoSource;
use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoExcludeRequest {
    pub source: RepoSource,
}

impl RepoExcludeRequest {
    pub fn new_fs(path: impl Into<PathBuf>) -> Self {
        Self {
            source: RepoSource::Fs(path.into()),
        }
    }

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
            let mut request = builder.init_root::<repo_capnp::repo_exclude_request::Builder>();
            let mut source = request.reborrow().init_source();
            match &self.source {
                RepoSource::Fs(path) => {
                    source.set_fs(path.to_string_lossy().as_ref());
                }
                RepoSource::Git { repo_url, repo_ref } => {
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
        use crate::repo_capnp::repo_exclude_request::source::Which;

        let reader = decode_message(data)?;
        let request = reader.get_root::<repo_capnp::repo_exclude_request::Reader>()?;
        let source = match request.get_source().which()? {
            Which::Fs(path) => RepoSource::Fs(crate::encoding::decode_fs_path(
                path?.to_str()?,
                "RepoExcludeRequest.source.fs",
            )?),
            Which::Git(git) => {
                let git = git?;
                let repo_url = git.get_repo_url()?.to_str()?.to_owned();
                let repo_ref = optional_text(git.get_repo_ref()?.to_str()?);
                RepoSource::Git { repo_url, repo_ref }
            }
            Which::Url(url) => RepoSource::Url(url?.to_str()?.to_owned()),
        };
        Ok(Self { source })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoExcludeResponse {
    pub success: bool,
    pub error_message: String,
}

impl RepoExcludeResponse {
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
            let mut response = builder.init_root::<repo_capnp::repo_exclude_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_exclude_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_request_new_fs_round_trips() {
        let request = RepoExcludeRequest::new_fs("/abs/repo");
        assert_eq!(request.source, RepoSource::Fs(PathBuf::from("/abs/repo")));
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn exclude_request_new_git_round_trips_with_ref() {
        let request =
            RepoExcludeRequest::new_git("https://github.com/org/repo", Some("main".to_owned()));
        assert_eq!(
            request.source,
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_owned(),
                repo_ref: Some("main".to_owned()),
            }
        );
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn exclude_request_new_git_round_trips_without_ref() {
        // An absent ref encodes as the empty string and decodes back to `None`.
        let request = RepoExcludeRequest::new_git("https://github.com/org/repo", None);
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert_eq!(
            decoded.source,
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_owned(),
                repo_ref: None,
            }
        );
    }

    #[test]
    fn exclude_request_new_url_round_trips() {
        let request = RepoExcludeRequest::new_url("https://example.com/packages");
        assert_eq!(
            request.source,
            RepoSource::Url("https://example.com/packages".to_owned())
        );
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn exclude_response_success_round_trips() {
        let response = RepoExcludeResponse::success();
        let payload = response.encode().expect("encode");
        let decoded = RepoExcludeResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.success);
        assert!(decoded.error_message.is_empty());
    }

    #[test]
    fn exclude_response_failure_round_trips() {
        let response = RepoExcludeResponse::failure("not found");
        let payload = response.encode().expect("encode");
        let decoded = RepoExcludeResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(!decoded.success);
        assert_eq!(decoded.error_message, "not found");
    }

    #[test]
    fn exclude_request_decode_rejects_malformed_bytes() {
        RepoExcludeRequest::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }

    #[test]
    fn exclude_response_decode_rejects_malformed_bytes() {
        RepoExcludeResponse::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }
}
