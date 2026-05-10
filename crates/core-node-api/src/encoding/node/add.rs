//! Encoding types for the NodeAdd action (streaming version with feedback).

use crate::node_capnp;
use crate::{NonEmptyPayload, Payload, Result};
use capnp::message::Builder;
use gix_url::Url as GitUrl;
use std::path::PathBuf;

use super::builder::FeedbackStream;
use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSource {
    Fs(PathBuf),
    Git {
        repo_url: GitUrl,
        repo_path: String,
        repo_ref: Option<String>,
    },
    // Only .tzst (.tar.zstd) archives are supported for the moment
    Http {
        url: url::Url,
        sha256: Option<String>,
    },
    /// Reference a node by `(name, tag)`; the daemon resolves it and
    /// its transitive dependencies against the repo cache
    /// (`~/.peppy/cache/nodes.json5`) and adds them as one batch.
    ///
    /// Dep-level variant overrides travel with the source so they're
    /// unrepresentable on non-repo sources.
    RepoNode {
        name: String,
        tag: String,
        dep_variant_overrides: Vec<DepVariantOverride>,
    },
}

/// Per-dependency variant override for `RepoNode` batch adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepVariantOverride {
    pub name: String,
    pub tag: String,
    pub variant: String,
}

impl NodeSource {
    /// Validated convenience constructor for a `RepoNode` with no dep
    /// overrides. Applies the same name/tag validation as
    /// [`Self::decode_repo_node`] so callers cannot build an unsafe
    /// source that would later be rejected on the wire.
    pub fn repo_node(name: impl AsRef<str>, tag: impl AsRef<str>) -> Result<Self> {
        Self::decode_repo_node(name.as_ref(), tag.as_ref(), Vec::new())
    }

    /// Replaces the dep-override list wholesale on a `RepoNode` source,
    /// validating each override with the same rules as
    /// [`Self::decode_repo_node`]. No-op for every other source kind.
    pub fn with_dep_variant_overrides(
        mut self,
        overrides: Vec<DepVariantOverride>,
    ) -> Result<Self> {
        if let Self::RepoNode {
            ref mut dep_variant_overrides,
            ..
        } = self
        {
            for ov in &overrides {
                validate_repo_node_name(&ov.name, "repo-node dep override name")?;
                validate_repo_node_tag(&ov.tag, "repo-node dep override tag")?;
                validate_repo_node_name(&ov.variant, "repo-node dep override variant")?;
            }
            *dep_variant_overrides = overrides;
        }
        Ok(self)
    }
}

impl NodeSource {
    pub fn decode_fs(path: &str) -> Result<Self> {
        Ok(Self::Fs(crate::encoding::decode_fs_path(
            path,
            "NodeSource.fs",
        )?))
    }

    pub fn decode_git(repo_url_str: &str, repo_path: &str, repo_ref: &str) -> Result<Self> {
        let repo_url = GitUrl::try_from(repo_url_str)
            .map_err(|e| crate::Error::Decoding(format!("invalid git URL: {}", e)))?;
        Ok(Self::Git {
            repo_url,
            repo_path: repo_path.to_owned(),
            repo_ref: optional_text(repo_ref.trim()),
        })
    }

    pub(crate) fn normalize_http_sha256(sha256: Option<&str>) -> Option<String> {
        sha256
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub fn decode_http(url_str: &str, sha256: Option<&str>) -> Result<Self> {
        let url = url::Url::parse(url_str)
            .map_err(|e| crate::Error::Decoding(format!("invalid HTTP URL: {}", e)))?;
        Ok(Self::Http {
            url,
            sha256: Self::normalize_http_sha256(sha256),
        })
    }

    pub fn decode_repo_node(
        name: &str,
        tag: &str,
        dep_variant_overrides: Vec<DepVariantOverride>,
    ) -> Result<Self> {
        validate_repo_node_name(name, "repo-node name")?;
        validate_repo_node_tag(tag, "repo-node tag")?;
        for ov in &dep_variant_overrides {
            validate_repo_node_name(&ov.name, "repo-node dep override name")?;
            validate_repo_node_tag(&ov.tag, "repo-node dep override tag")?;
            validate_repo_node_name(&ov.variant, "repo-node dep override variant")?;
        }
        Ok(Self::RepoNode {
            name: name.to_owned(),
            tag: tag.to_owned(),
            dep_variant_overrides,
        })
    }
}

fn validate_repo_node_name(value: &str, label: &str) -> Result<()> {
    config::repo_node_id::validate_repo_node_name(value, label).map_err(crate::Error::Decoding)
}

fn validate_repo_node_tag(tag: &str, label: &str) -> Result<()> {
    config::repo_node_id::validate_repo_node_tag(tag, label).map_err(crate::Error::Decoding)
}

/// Goal message for the NodeAdd action.
pub struct NodeAddGoal {
    pub source: NodeSource,
    pub git_hash: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub variant: Option<NodeSource>,
    pub force: bool,
}

impl NodeAddGoal {
    /// Creates a new NodeAddGoal from a NodeSource.
    pub fn from_source(source: NodeSource, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source,
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
            variant: None,
            force: false,
        }
    }

    /// Creates a new NodeAddGoal from a filesystem path.
    pub fn new(path: impl Into<PathBuf>, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self::from_source(NodeSource::Fs(path.into()), git_hash, timeout_secs)
    }

    /// Creates a new NodeAddGoal from a Git repository with an optional ref (tag/branch/commit).
    pub fn new_git(
        repo_url: GitUrl,
        repo_path: impl Into<String>,
        repo_ref: Option<String>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self::from_source(
            NodeSource::Git {
                repo_url,
                repo_path: repo_path.into(),
                repo_ref,
            },
            git_hash,
            timeout_secs,
        )
    }

    /// Creates a new NodeAddGoal from an HTTP URL (for .tzst archives).
    pub fn new_http(
        url: url::Url,
        sha256: Option<String>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self::from_source(NodeSource::Http { url, sha256 }, git_hash, timeout_secs)
    }

    /// Creates a new NodeAddGoal that targets a node by `(name, tag)`
    /// against the daemon's repo cache (no dep overrides). Returns an
    /// error when the name or tag fails the repo-node validation rules.
    pub fn new_repo_node(
        name: impl AsRef<str>,
        tag: impl AsRef<str>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        Ok(Self::from_source(
            NodeSource::repo_node(name, tag)?,
            git_hash,
            timeout_secs,
        ))
    }

    /// Builds a goal for in-process execution that bypasses the action-loop
    /// gate (see `services::stack::launch::add_node_directly`). The
    /// `timeout_secs` field feeds the gate's busy-reporting and is unread on
    /// this path, so it is zero by construction.
    pub fn for_internal_execution(source: NodeSource, git_hash: impl Into<String>) -> Self {
        Self::from_source(source, git_hash, 0)
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn with_variant_name(mut self, name: impl Into<String>) -> Self {
        self.variant = Some(NodeSource::Fs(PathBuf::from(name.into())));
        self
    }

    pub fn with_variant_source(mut self, source: NodeSource) -> Self {
        self.variant = Some(source);
        self
    }

    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Returns the filesystem path if the source is `Fs`, otherwise `None`.
    pub fn fs_path(&self) -> Option<&PathBuf> {
        match &self.source {
            NodeSource::Fs(path) => Some(path),
            NodeSource::Git { .. } | NodeSource::Http { .. } | NodeSource::RepoNode { .. } => None,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_add_goal::Builder>();
            goal.set_git_hash(&self.git_hash);
            let mut source = goal.reborrow().init_source();
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
                        goal.reborrow().set_http_sha256(&digest);
                    }
                }
                NodeSource::RepoNode {
                    name,
                    tag,
                    dep_variant_overrides,
                } => {
                    let mut repo = source.init_repo_node();
                    repo.set_name(name);
                    repo.set_tag(tag);
                    let override_count = capnp_list_len(
                        dep_variant_overrides.len(),
                        "NodeAddGoal.dep_variant_overrides",
                    )?;
                    let mut overrides = repo.reborrow().init_dep_variant_overrides(override_count);
                    for (idx, ov) in dep_variant_overrides.iter().enumerate() {
                        let mut entry = overrides.reborrow().get(idx as u32);
                        entry.set_name(&ov.name);
                        entry.set_tag(&ov.tag);
                        entry.set_variant(&ov.variant);
                    }
                }
            }

            let env_var_count = capnp_list_len(self.env_vars.len(), "NodeAddGoal.env_vars")?;
            let mut env_vars = goal.reborrow().init_env_vars(env_var_count);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);
            goal.reborrow().set_force(self.force);

            if let Some(ref variant) = self.variant {
                let mut variant_builder = goal.reborrow().init_variant();
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
                    NodeSource::RepoNode { .. } => {
                        return Err(crate::Error::RepoNodeNotValidVariantSource);
                    }
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::node_capnp::node_add_goal::source::Which;
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_add_goal::Reader>()?;
        let source = match goal.get_source().which()? {
            Which::Fs(fs) => NodeSource::decode_fs(fs?.to_str()?)?,
            Which::Git(git) => {
                let git = git?;
                NodeSource::decode_git(
                    git.get_repo_url()?.to_str()?,
                    git.get_repo_path()?.to_str()?,
                    git.get_repo_ref()?.to_str()?,
                )?
            }
            Which::Http(http) => {
                NodeSource::decode_http(http?.to_str()?, Some(goal.get_http_sha256()?.to_str()?))?
            }
            Which::RepoNode(repo) => {
                let repo = repo?;
                let overrides_reader = repo.get_dep_variant_overrides()?;
                let mut overrides = Vec::with_capacity(overrides_reader.len() as usize);
                for idx in 0..overrides_reader.len() {
                    let entry = overrides_reader.get(idx);
                    overrides.push(DepVariantOverride {
                        name: entry.get_name()?.to_str()?.to_owned(),
                        tag: entry.get_tag()?.to_str()?.to_owned(),
                        variant: entry.get_variant()?.to_str()?.to_owned(),
                    });
                }
                NodeSource::decode_repo_node(
                    repo.get_name()?.to_str()?,
                    repo.get_tag()?.to_str()?,
                    overrides,
                )?
            }
        };

        let env_vars_reader = goal.get_env_vars()?;
        let mut env_vars = Vec::with_capacity(env_vars_reader.len() as usize);
        for idx in 0..env_vars_reader.len() {
            let env_var = env_vars_reader.get(idx);
            env_vars.push((
                env_var.get_key()?.to_str()?.to_owned(),
                env_var.get_value()?.to_str()?.to_owned(),
            ));
        }

        let variant = if goal.has_variant() {
            use crate::node_capnp::node_add_variant_source::source::Which;
            let variant_reader = goal.get_variant()?;
            match variant_reader.get_source().which()? {
                Which::Fs(fs) => Some(NodeSource::decode_fs(fs?.to_str()?)?),
                Which::Git(git) => {
                    let git = git?;
                    Some(NodeSource::decode_git(
                        git.get_repo_url()?.to_str()?,
                        git.get_repo_path()?.to_str()?,
                        git.get_repo_ref()?.to_str()?,
                    )?)
                }
                Which::Http(http) => Some(NodeSource::decode_http(
                    http?.to_str()?,
                    Some(variant_reader.get_http_sha256()?.to_str()?),
                )?),
            }
        } else {
            None
        };

        Ok(Self {
            source,
            git_hash: goal.get_git_hash()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
            variant,
            force: goal.get_force(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_fs_rejects_empty_path() {
        let result = NodeSource::decode_fs("");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("NodeSource.fs") && err_msg.contains("empty"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn decode_fs_accepts_non_empty_path() {
        let source = NodeSource::decode_fs("/some/path").expect("should accept non-empty path");
        assert_eq!(source, NodeSource::Fs(PathBuf::from("/some/path")));
    }

    #[test]
    fn node_add_goal_decode_rejects_empty_fs_source() {
        // Encode a goal with an empty Fs path (bypass decode_fs by using NodeSource::Fs directly)
        let goal = NodeAddGoal {
            source: NodeSource::Fs(PathBuf::from("")),
            git_hash: "hash".to_owned(),
            env_vars: vec![],
            timeout_secs: 30,
            variant: None,
            force: false,
        };
        let encoded = goal.encode().expect("encoding should succeed");
        let result = NodeAddGoal::decode(&encoded);
        assert!(result.is_err(), "decoding an empty Fs source should fail");
    }

    #[test]
    fn node_add_goal_http_source_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/node.tar.zst").unwrap();
        let sha256 = "a".repeat(64);

        let encoded = NodeAddGoal::new_http(url.clone(), Some(sha256.clone()), "git-hash", 42)
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.source,
            NodeSource::Http {
                url,
                sha256: Some(sha256)
            }
        );
    }

    #[test]
    fn node_add_goal_repo_node_source_roundtrips() {
        let encoded = NodeAddGoal::new_repo_node("camera", "0.1.0", "hash", 42)
            .expect("repo_node constructor should accept valid inputs")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
        assert_eq!(
            decoded.source,
            NodeSource::RepoNode {
                name: "camera".to_owned(),
                tag: "0.1.0".to_owned(),
                dep_variant_overrides: vec![],
            }
        );
    }

    #[test]
    fn node_add_goal_dep_variant_overrides_roundtrip() {
        let overrides = vec![
            DepVariantOverride {
                name: "uvc_camera".to_owned(),
                tag: "0.1.0".to_owned(),
                variant: "mock-python".to_owned(),
            },
            DepVariantOverride {
                name: "lidar".to_owned(),
                tag: "2.0.0".to_owned(),
                variant: "sim".to_owned(),
            },
        ];
        let source = NodeSource::repo_node("target", "1.0.0")
            .expect("repo_node constructor should accept valid inputs")
            .with_dep_variant_overrides(overrides)
            .expect("with_dep_variant_overrides should accept valid overrides");
        let encoded = NodeAddGoal::from_source(source, "hash", 42)
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
        let NodeSource::RepoNode {
            ref dep_variant_overrides,
            ..
        } = decoded.source
        else {
            panic!("expected RepoNode source, got {:?}", decoded.source);
        };
        assert_eq!(dep_variant_overrides.len(), 2);
        assert_eq!(dep_variant_overrides[0].name, "uvc_camera");
        assert_eq!(dep_variant_overrides[0].tag, "0.1.0");
        assert_eq!(dep_variant_overrides[0].variant, "mock-python");
        assert_eq!(dep_variant_overrides[1].variant, "sim");
    }

    #[test]
    fn with_dep_variant_overrides_is_noop_on_non_repo_source() {
        let overrides = vec![DepVariantOverride {
            name: "a".to_owned(),
            tag: "1.0".to_owned(),
            variant: "v".to_owned(),
        }];
        let source = NodeSource::Fs(PathBuf::from("/tmp/x"))
            .with_dep_variant_overrides(overrides)
            .expect("non-repo sources skip override validation");
        assert!(matches!(source, NodeSource::Fs(_)));
    }

    #[test]
    fn repo_node_rejects_invalid_name() {
        assert!(NodeSource::repo_node("../etc", "1.0").is_err());
        assert!(NodeSource::repo_node("bad name", "1.0").is_err());
    }

    #[test]
    fn repo_node_rejects_invalid_tag() {
        assert!(NodeSource::repo_node("node", "").is_err());
        assert!(NodeSource::repo_node("node", "..").is_err());
    }

    #[test]
    fn with_dep_variant_overrides_rejects_invalid_override() {
        let base = NodeSource::repo_node("node", "1.0").expect("valid");
        let bad = vec![DepVariantOverride {
            name: "../evil".to_owned(),
            tag: "0.1.0".to_owned(),
            variant: "v".to_owned(),
        }];
        assert!(base.with_dep_variant_overrides(bad).is_err());
    }

    #[test]
    fn decode_repo_node_rejects_empty_name() {
        assert!(NodeSource::decode_repo_node("", "0.1.0", vec![]).is_err());
    }

    #[test]
    fn decode_repo_node_rejects_empty_tag() {
        assert!(NodeSource::decode_repo_node("node", "", vec![]).is_err());
    }

    #[test]
    fn decode_repo_node_rejects_path_traversal_in_name() {
        for name in [
            "../etc",
            "a/b",
            "a\\b",
            "..",
            ".hidden",
            " leading",
            "name with space",
        ] {
            assert!(
                NodeSource::decode_repo_node(name, "0.1.0", vec![]).is_err(),
                "name `{name}` should be rejected"
            );
        }
    }

    #[test]
    fn decode_repo_node_rejects_path_traversal_in_tag() {
        for tag in [
            "../etc",
            "a/b",
            "a\\b",
            "..",
            ".hidden",
            "1..2",
            "tag with space",
        ] {
            assert!(
                NodeSource::decode_repo_node("node", tag, vec![]).is_err(),
                "tag `{tag}` should be rejected"
            );
        }
    }

    #[test]
    fn decode_repo_node_rejects_unsafe_dep_override() {
        let overrides = vec![DepVariantOverride {
            name: "../evil".to_owned(),
            tag: "0.1.0".to_owned(),
            variant: "v".to_owned(),
        }];
        assert!(NodeSource::decode_repo_node("node", "0.1.0", overrides).is_err());
    }

    #[test]
    fn node_add_goal_http_variant_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/variant.tar.zst").unwrap();
        let sha256 = "b".repeat(64);

        let encoded = NodeAddGoal::new("/some/path", "git-hash", 42)
            .with_variant_source(NodeSource::Http {
                url: url.clone(),
                sha256: Some(sha256.clone()),
            })
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.variant,
            Some(NodeSource::Http {
                url,
                sha256: Some(sha256)
            })
        );
    }
}

/// Response to the NodeAdd goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl NodeAddGoalResponse {
    pub fn accepted(log_path: impl Into<PathBuf>) -> Self {
        Self {
            accepted: true,
            log_path: log_path.into(),
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            log_path: PathBuf::new(),
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_add_goal_response::Builder>();
            response.set_accepted(self.accepted);
            response.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_add_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the NodeAdd action.
/// Represents a single line of output from the build_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddFeedback {
    pub stream: FeedbackStream,
    /// The line of output
    pub line: String,
}

impl NodeAddFeedback {
    pub fn from_stream(stream: FeedbackStream, line: impl Into<String>) -> Self {
        Self {
            stream,
            line: line.into(),
        }
    }

    pub fn stdout(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stdout, line)
    }

    pub fn stderr(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stderr, line)
    }

    pub fn warning(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Warning, line)
    }

    pub fn is_stdout(&self) -> bool {
        self.stream == FeedbackStream::Stdout
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == FeedbackStream::Stderr
    }

    pub fn is_warning(&self) -> bool {
        self.stream == FeedbackStream::Warning
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<node_capnp::node_add_feedback::Builder>();
            feedback.set_stream(self.stream.to_capnp());
            feedback.set_line(&self.line);
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_add_feedback::Reader>()?;
        Ok(Self {
            stream: FeedbackStream::from_capnp(feedback.get_stream()?),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeAdd action.
///
/// Note: `node add` only registers the node config and stages a working
/// directory; the build artifact is produced by a separate `node build`
/// action and is therefore not part of this result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddResult {
    pub log_path: PathBuf,
    pub success: bool,
    pub error_message: Option<String>,
    pub node_name: Option<String>,
    pub node_tag: Option<String>,
}

impl NodeAddResult {
    pub fn success(
        log_path: impl Into<PathBuf>,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
    ) -> Self {
        Self {
            log_path: log_path.into(),
            success: true,
            error_message: None,
            node_name: Some(node_name.into()),
            node_tag: Some(node_tag.into()),
        }
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self {
            log_path: log_path.into(),
            success: false,
            error_message: Some(error_message.into()),
            node_name: None,
            node_tag: None,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_add_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref node_name) = self.node_name {
                result.set_node_name(node_name);
            }
            if let Some(ref node_tag) = self.node_tag {
                result.set_node_tag(node_tag);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_add_result::Reader>()?;
        let log_path = PathBuf::from(result.get_log_path()?.to_str()?);
        Ok(Self {
            log_path,
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            node_name: optional_text(result.get_node_name()?.to_str()?),
            node_tag: optional_text(result.get_node_tag()?.to_str()?),
        })
    }
}
