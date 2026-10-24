use std::path::PathBuf;

use capnp::message::Builder;

use crate::repo_capnp;
use crate::{Payload, Result};

use crate::encoding::RepoSource;
use crate::encoding::{decode_message, encode_fs_path, encode_message};

use super::add::{read_git_source, write_git_source};
use super::git_ref::GitRepoRef;

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

    pub fn new_git(repo_url: impl Into<String>, repo_ref: GitRepoRef) -> Self {
        Self {
            source: RepoSource::Git {
                repo_url: repo_url.into(),
                repo_ref,
            },
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<repo_capnp::repo_exclude_request::Builder>();
            let mut source = request.reborrow().init_source();
            match &self.source {
                RepoSource::Fs(path) => {
                    source.set_fs(encode_fs_path(path, "RepoExcludeRequest.source.fs")?);
                }
                RepoSource::Git { repo_url, repo_ref } => {
                    write_git_source(source.init_git(), repo_url, repo_ref);
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
            Which::Git(git) => read_git_source(git?)?,
        };
        Ok(Self { source })
    }
}

/// The response of a configuration edit that landed. A failed edit is an
/// error reply, with a failure kind and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoExcludeResponse {
    /// Problems from the re-index that follows the change: whether the
    /// re-read that made the edit take effect worked. Empty when it did.
    pub refresh_report: String,
}

impl RepoExcludeResponse {
    /// The edit landed and the re-index that follows it worked.
    pub fn refreshed() -> Self {
        Self::with_refresh_report("")
    }

    /// The edit landed but the re-index that follows it reported problems,
    /// so a user on the recovery path can tell "your change applied, and
    /// here is what is still wrong" from an error reply.
    pub fn with_refresh_report(report: impl Into<String>) -> Self {
        Self {
            refresh_report: report.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_exclude_response::Builder>();
            response.set_refresh_report(&self.refresh_report);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_exclude_response::Reader>()?;
        Ok(Self {
            refresh_report: response.get_refresh_report()?.to_str()?.to_owned(),
        })
    }
}

impl crate::encoding::Wire for RepoExcludeRequest {
    type Root = crate::repo_capnp::repo_exclude_request::Owned;
}

impl crate::encoding::Wire for RepoExcludeResponse {
    type Root = crate::repo_capnp::repo_exclude_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon matches `RepoSource::Fs` against its repositories as sent,
    /// so a path that is not UTF-8 is refused instead of reaching it with its
    /// invalid bytes replaced.
    #[cfg(unix)]
    #[test]
    fn exclude_request_encode_refuses_an_fs_source_that_is_not_utf8() {
        let request = RepoExcludeRequest::new_fs(crate::encoding::non_utf8_path());
        let refusal = crate::encoding::encoding_error(request.encode());
        assert!(
            refusal.contains("RepoExcludeRequest.source.fs is not valid UTF-8"),
            "{refusal}"
        );
    }

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
        let request = RepoExcludeRequest::new_git(
            "https://github.com/org/repo",
            GitRepoRef::Named("main".to_owned()),
        );
        assert_eq!(
            request.source,
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_owned(),
                repo_ref: GitRepoRef::Named("main".to_owned()),
            }
        );
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn exclude_request_new_git_round_trips_without_ref() {
        // An absent ref encodes as the empty string and decodes back to the
        // remote HEAD.
        let request =
            RepoExcludeRequest::new_git("https://github.com/org/repo", GitRepoRef::RemoteHead);
        let payload = request.encode().expect("encode");
        let decoded = RepoExcludeRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert_eq!(
            decoded.source,
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_owned(),
                repo_ref: GitRepoRef::RemoteHead,
            }
        );
    }

    #[test]
    fn exclude_response_refreshed_round_trips() {
        let response = RepoExcludeResponse::refreshed();
        let payload = response.encode().expect("encode");
        let decoded = RepoExcludeResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.refresh_report.is_empty());
    }

    #[test]
    fn exclude_response_with_refresh_report_round_trips() {
        // The edit landed and the re-index complained: the report has to
        // survive the wire, or the recovery path reads as a clean success.
        let response = RepoExcludeResponse::with_refresh_report("repo 3 conflict: robot:v1 twice");
        let payload = response.encode().expect("encode");
        let decoded = RepoExcludeResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert_eq!(decoded.refresh_report, "repo 3 conflict: robot:v1 twice");
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
