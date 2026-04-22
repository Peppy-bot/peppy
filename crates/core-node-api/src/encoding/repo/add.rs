use std::fmt;
use std::path::PathBuf;

use capnp::message::Builder;

use crate::Result;
use crate::repo_capnp;

use crate::encoding::{decode_message, encode_message};

/// Discriminant for the type of repository source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoSourceKind {
    Fs,
    Git,
    Url,
}

impl RepoSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoSourceKind::Fs => "fs",
            RepoSourceKind::Git => "git",
            RepoSourceKind::Url => "url",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fs" => Some(RepoSourceKind::Fs),
            "git" => Some(RepoSourceKind::Git),
            "url" => Some(RepoSourceKind::Url),
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
    Url(String),
}

impl RepoSource {
    /// The canonical identity string used for duplicate detection and exclusion matching.
    ///
    /// - `Fs`: canonicalized (absolute, symlink-resolved) when possible, so that
    ///   `./repo` and `/abs/path/to/repo` produce the same identity. Falls back
    ///   to the raw string when the path does not exist.
    /// - `Git`: `repo_url@repo_ref` when a ref is present, otherwise just the
    ///   url — so that the same repo pinned to different refs is not collapsed
    ///   into a single identity.
    /// - `Url`: the url as-is.
    pub fn identity(&self) -> String {
        match self {
            RepoSource::Fs(path) => std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned()),
            RepoSource::Git { repo_url, repo_ref } => match repo_ref {
                Some(r) if !r.is_empty() => format!("{repo_url}@{r}"),
                _ => repo_url.clone(),
            },
            RepoSource::Url(url) => url.clone(),
        }
    }

    pub fn kind(&self) -> RepoSourceKind {
        match self {
            RepoSource::Fs(_) => RepoSourceKind::Fs,
            RepoSource::Git { .. } => RepoSourceKind::Git,
            RepoSource::Url(_) => RepoSourceKind::Url,
        }
    }

    /// Human-readable label for CLI output.
    ///
    /// - `Fs`: path as-written
    /// - `Git`: `"url (ref: r)"` when a ref is configured, else `"url"`. Code
    ///   paths that have access to the actual checked-out ref (e.g. the
    ///   packages cache) may prefer to build their own label.
    /// - `Url`: the url as-is
    pub fn display_label(&self) -> String {
        match self {
            RepoSource::Fs(path) => path.to_string_lossy().into_owned(),
            RepoSource::Git { repo_url, repo_ref } => match repo_ref {
                Some(r) if !r.is_empty() => format!("{repo_url} (ref: {r})"),
                _ => repo_url.clone(),
            },
            RepoSource::Url(url) => url.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAddRequest {
    pub source: RepoSource,
    pub top: bool,
}

impl RepoAddRequest {
    pub fn new_fs(path: impl Into<PathBuf>) -> Self {
        Self {
            source: RepoSource::Fs(path.into()),
            top: false,
        }
    }

    pub fn new_git(repo_url: impl Into<String>, repo_ref: Option<String>) -> Self {
        Self {
            source: RepoSource::Git {
                repo_url: repo_url.into(),
                repo_ref,
            },
            top: false,
        }
    }

    pub fn new_url(url: impl Into<String>) -> Self {
        Self {
            source: RepoSource::Url(url.into()),
            top: false,
        }
    }

    pub fn with_top(mut self, top: bool) -> Self {
        self.top = top;
        self
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<repo_capnp::repo_add_request::Builder>();
            request.set_top(self.top);
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
        use crate::repo_capnp::repo_add_request::source::Which;

        let reader = decode_message(data)?;
        let request = reader.get_root::<repo_capnp::repo_add_request::Reader>()?;
        let top = request.get_top();
        let source = match request.get_source().which()? {
            Which::Fs(path) => RepoSource::Fs(PathBuf::from(path?.to_str()?)),
            Which::Git(git) => {
                let git = git?;
                let repo_url = git.get_repo_url()?.to_str()?.to_owned();
                let repo_ref_str = git.get_repo_ref()?.to_str()?.to_owned();
                let repo_ref = if repo_ref_str.is_empty() {
                    None
                } else {
                    Some(repo_ref_str)
                };
                RepoSource::Git { repo_url, repo_ref }
            }
            Which::Url(url) => RepoSource::Url(url?.to_str()?.to_owned()),
        };
        Ok(Self { source, top })
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

    pub fn encode(&self) -> Result<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_git_distinguishes_refs() {
        let a = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("main".to_string()),
        };
        let b = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("dev".to_string()),
        };
        assert_ne!(a.identity(), b.identity());
        assert!(a.identity().contains("main"));
        assert!(b.identity().contains("dev"));
    }

    #[test]
    fn identity_git_without_ref_matches_url() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: None,
        };
        assert_eq!(src.identity(), "https://github.com/org/repo");
    }

    #[test]
    fn identity_git_empty_ref_matches_url() {
        // Treat empty ref as "no ref" so it matches legacy entries without a ref.
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some(String::new()),
        };
        assert_eq!(src.identity(), "https://github.com/org/repo");
    }

    #[test]
    fn identity_fs_canonicalizes_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();

        // Build a non-canonical spelling via the symlink-prone /tmp or an
        // equivalent relative path: construct `<canonical>/../<basename>`.
        let parent = canonical.parent().unwrap();
        let name = canonical.file_name().unwrap();
        let roundabout = parent
            .join("..")
            .join(parent.file_name().unwrap())
            .join(name);

        let raw = RepoSource::Fs(roundabout.clone());
        let canon = RepoSource::Fs(canonical.clone());
        assert_eq!(
            raw.identity(),
            canon.identity(),
            "canonicalization must collapse equivalent paths"
        );
    }

    #[test]
    fn identity_fs_nonexistent_falls_back_to_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp
            .path()
            .join("definitely")
            .join("does-not-exist")
            .join("xyz");
        let src = RepoSource::Fs(missing.clone());
        assert_eq!(src.identity(), missing.to_string_lossy().into_owned());
    }

    #[test]
    fn identity_url_is_unchanged() {
        let src = RepoSource::Url("https://example.com/packages".to_string());
        assert_eq!(src.identity(), "https://example.com/packages");
    }
}
