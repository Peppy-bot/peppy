use std::fmt;
use std::path::PathBuf;

use capnp::message::Builder;

use crate::repo_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

/// Discriminant for the type of repository source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoSourceKind {
    Fs,
    Git,
}

impl RepoSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoSourceKind::Fs => "fs",
            RepoSourceKind::Git => "git",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fs" => Some(RepoSourceKind::Fs),
            "git" => Some(RepoSourceKind::Git),
            _ => None,
        }
    }
}

impl fmt::Display for RepoSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSource {
    Fs(PathBuf),
    Git {
        repo_url: String,
        repo_ref: Option<String>,
    },
}

impl RepoSource {
    /// The discriminant kind of this source.
    ///
    /// The canonical *identity* string (used for duplicate detection and
    /// exclusion matching) is intentionally not computed here: the `Fs` arm
    /// canonicalizes against the real filesystem, which is the daemon's job —
    /// see `core-node`'s `services::repo::source_identity`.
    pub fn kind(&self) -> RepoSourceKind {
        match self {
            RepoSource::Fs(_) => RepoSourceKind::Fs,
            RepoSource::Git { .. } => RepoSourceKind::Git,
        }
    }

    /// Human-readable label for CLI output.
    ///
    /// - `Fs`: path as-written
    /// - `Git`: `"url (ref: r)"` when a ref is configured, else `"url"`. Code
    ///   paths that have access to the actual checked-out ref (e.g. the
    ///   packages cache) may prefer to build their own label.
    pub fn display_label(&self) -> String {
        match self {
            RepoSource::Fs(path) => path.to_string_lossy().into_owned(),
            RepoSource::Git { repo_url, repo_ref } => match repo_ref {
                Some(r) if !r.is_empty() => format!("{repo_url} (ref: {r})"),
                _ => repo_url.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAddRequest {
    pub source: RepoSource,
    pub top: bool,
    /// Explicit repository id to register the source under, or `None` to
    /// auto-assign (`max + 1`, or `min - 1` when `top`). Callers that pin an
    /// id should take it from a reserved band (>= 2000) so it can never
    /// collide with a default a future peppy release ships.
    pub id: Option<u64>,
}

impl RepoAddRequest {
    pub fn new_fs(path: impl Into<PathBuf>) -> Self {
        Self {
            source: RepoSource::Fs(path.into()),
            top: false,
            id: None,
        }
    }

    pub fn new_git(repo_url: impl Into<String>, repo_ref: Option<String>) -> Self {
        Self {
            source: RepoSource::Git {
                repo_url: repo_url.into(),
                repo_ref,
            },
            top: false,
            id: None,
        }
    }

    pub fn with_top(mut self, top: bool) -> Self {
        self.top = top;
        self
    }

    pub fn with_id(mut self, id: u64) -> Self {
        self.id = Some(id);
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<repo_capnp::repo_add_request::Builder>();
            request.set_top(self.top);
            if let Some(id) = self.id {
                request.reborrow().init_id().set_explicit(id);
            }
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
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::repo_capnp::repo_add_request::id::Which as IdWhich;
        use crate::repo_capnp::repo_add_request::source::Which;

        let reader = decode_message(data)?;
        let request = reader.get_root::<repo_capnp::repo_add_request::Reader>()?;
        let top = request.get_top();
        // An `auto` discriminant is what a sender that never set the union
        // leaves on the wire, so ids decode as `None` for every pre-id peer.
        let id = match request.get_id().which()? {
            IdWhich::Auto(()) => None,
            IdWhich::Explicit(id) => Some(id),
        };
        let source = match request.get_source().which()? {
            Which::Fs(path) => RepoSource::Fs(crate::encoding::decode_fs_path(
                path?.to_str()?,
                "RepoAddRequest.source.fs",
            )?),
            Which::Git(git) => {
                let git = git?;
                let repo_url = git.get_repo_url()?.to_str()?.to_owned();
                let repo_ref = optional_text(git.get_repo_ref()?.to_str()?);
                RepoSource::Git { repo_url, repo_ref }
            }
        };
        Ok(Self { source, top, id })
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

impl crate::encoding::Wire for RepoAddRequest {
    type Root = crate::repo_capnp::repo_add_request::Owned;
}

impl crate::encoding::Wire for RepoAddResponse {
    type Root = crate::repo_capnp::repo_add_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_as_str_parse_roundtrips() {
        for kind in [RepoSourceKind::Fs, RepoSourceKind::Git] {
            assert_eq!(RepoSourceKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn source_kind_as_str_values() {
        assert_eq!(RepoSourceKind::Fs.as_str(), "fs");
        assert_eq!(RepoSourceKind::Git.as_str(), "git");
    }

    #[test]
    fn source_kind_parse_rejects_unknown() {
        assert_eq!(RepoSourceKind::parse("bogus"), None);
        assert_eq!(RepoSourceKind::parse(""), None);
        assert_eq!(RepoSourceKind::parse("Fs"), None);
    }

    #[test]
    fn source_kind_display_matches_as_str() {
        assert_eq!(RepoSourceKind::Git.to_string(), "git");
    }

    #[test]
    fn source_kind_reports_variant() {
        assert_eq!(
            RepoSource::Fs(PathBuf::from("/abs/repo")).kind(),
            RepoSourceKind::Fs
        );
        assert_eq!(
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_string(),
                repo_ref: None,
            }
            .kind(),
            RepoSourceKind::Git
        );
    }

    #[test]
    fn source_display_label_fs_is_path() {
        let src = RepoSource::Fs(PathBuf::from("/abs/path/to/repo"));
        assert_eq!(src.display_label(), "/abs/path/to/repo");
    }

    #[test]
    fn source_display_label_git_with_ref() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("main".to_string()),
        };
        assert_eq!(
            src.display_label(),
            "https://github.com/org/repo (ref: main)"
        );
    }

    #[test]
    fn source_display_label_git_without_ref() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: None,
        };
        assert_eq!(src.display_label(), "https://github.com/org/repo");
    }

    #[test]
    fn source_display_label_git_empty_ref() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some(String::new()),
        };
        assert_eq!(src.display_label(), "https://github.com/org/repo");
    }

    #[test]
    fn add_request_new_fs_defaults_top_false() {
        let request = RepoAddRequest::new_fs("/abs/path/to/repo");
        assert_eq!(
            request.source,
            RepoSource::Fs(PathBuf::from("/abs/path/to/repo"))
        );
        assert!(!request.top);
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode"), request);
    }

    #[test]
    fn add_request_new_git_with_ref_roundtrips() {
        let request =
            RepoAddRequest::new_git("https://github.com/org/repo", Some("main".to_string()));
        assert_eq!(
            request.source,
            RepoSource::Git {
                repo_url: "https://github.com/org/repo".to_string(),
                repo_ref: Some("main".to_string()),
            }
        );
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode"), request);
    }

    #[test]
    fn add_request_explicit_id_roundtrips() {
        let request = RepoAddRequest::new_fs("/abs/path/to/repo").with_id(2000);
        assert_eq!(request.id, Some(2000));
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode"), request);
    }

    #[test]
    fn add_request_without_id_decodes_as_auto() {
        // A request that never pins an id — including bytes a pre-id sender
        // produced — must decode with `id: None`, never a bogus explicit id.
        let request = RepoAddRequest::new_git("https://github.com/org/repo", None);
        assert_eq!(request.id, None);
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode").id, None);
    }

    #[test]
    fn add_request_new_git_without_ref_roundtrips() {
        // An empty ref on the wire decodes back to None via optional_text.
        let request = RepoAddRequest::new_git("https://github.com/org/repo", None);
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode"), request);
    }

    #[test]
    fn add_request_with_top_builder_sets_flag_and_roundtrips() {
        let request = RepoAddRequest::new_fs("/abs/path/to/repo").with_top(true);
        assert!(request.top);
        let bytes = request.encode().expect("encode");
        assert_eq!(RepoAddRequest::decode(&bytes).expect("decode"), request);
    }

    #[test]
    fn add_request_decode_rejects_malformed() {
        assert!(RepoAddRequest::decode(b"not capnp").is_err());
    }

    #[test]
    fn add_response_success_roundtrips() {
        let response = RepoAddResponse::success();
        assert!(response.success);
        assert_eq!(response.error_message, "");
        let bytes = response.encode().expect("encode");
        assert_eq!(RepoAddResponse::decode(&bytes).expect("decode"), response);
    }

    #[test]
    fn add_response_failure_roundtrips() {
        let response = RepoAddResponse::failure("already added");
        assert!(!response.success);
        assert_eq!(response.error_message, "already added");
        let bytes = response.encode().expect("encode");
        assert_eq!(RepoAddResponse::decode(&bytes).expect("decode"), response);
    }

    #[test]
    fn add_response_decode_rejects_malformed() {
        assert!(RepoAddResponse::decode(b"not capnp").is_err());
    }
}
