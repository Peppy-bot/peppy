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
    RepoNode {
        name: String,
        tag: String,
    },
}

impl NodeSource {
    /// Validated convenience constructor for a `RepoNode`. Applies the
    /// same name/tag validation as [`Self::decode_repo_node`] so callers
    /// cannot build an unsafe source that would later be rejected on the
    /// wire.
    pub fn repo_node(name: impl AsRef<str>, tag: impl AsRef<str>) -> Result<Self> {
        Self::decode_repo_node(name.as_ref(), tag.as_ref())
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

    pub fn decode_repo_node(name: &str, tag: &str) -> Result<Self> {
        validate_repo_node_name(name, "repo-node name")?;
        validate_repo_node_tag(tag, "repo-node tag")?;
        Ok(Self::RepoNode {
            name: name.to_owned(),
            tag: tag.to_owned(),
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
    /// against the daemon's repo cache. Returns an error when the name
    /// or tag fails the repo-node validation rules.
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
                    // Borrow the canonicalized URL bytes as text instead of
                    // allocating a second `String`: `from_utf8_lossy` returns a
                    // borrowed `Cow` for the valid-UTF-8 case (every real git
                    // URL), matching the lossy decoding `BString`'s `Display`
                    // already used.
                    git.set_repo_url(String::from_utf8_lossy(&repo_url.to_bstring()).as_ref());
                    git.set_repo_path(repo_path);
                    git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                }
                NodeSource::Http { url, sha256 } => {
                    source.set_http(url.as_str());
                    if let Some(digest) = NodeSource::normalize_http_sha256(sha256.as_deref()) {
                        goal.reborrow().set_http_sha256(&digest);
                    }
                }
                NodeSource::RepoNode { name, tag } => {
                    let mut repo = source.init_repo_node();
                    repo.set_name(name);
                    repo.set_tag(tag);
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
                NodeSource::decode_repo_node(repo.get_name()?.to_str()?, repo.get_tag()?.to_str()?)?
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

        Ok(Self {
            source,
            git_hash: goal.get_git_hash()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
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
        let encoded = NodeAddGoal::new_repo_node("camera", "v1", "hash", 42)
            .expect("repo_node constructor should accept valid inputs")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
        assert_eq!(
            decoded.source,
            NodeSource::RepoNode {
                name: "camera".to_owned(),
                tag: "v1".to_owned(),
            }
        );
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
    fn decode_repo_node_rejects_empty_name() {
        assert!(NodeSource::decode_repo_node("", "v1").is_err());
    }

    #[test]
    fn decode_repo_node_rejects_empty_tag() {
        assert!(NodeSource::decode_repo_node("node", "").is_err());
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
                NodeSource::decode_repo_node(name, "v1").is_err(),
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
                NodeSource::decode_repo_node("node", tag).is_err(),
                "tag `{tag}` should be rejected"
            );
        }
    }

    #[test]
    fn node_add_goal_with_env_vars_and_force_roundtrips() {
        let goal = NodeAddGoal::new("/some/node", "git-hash", 42)
            .with_env_vars(vec![
                ("KEY1".to_owned(), "VAL1".to_owned()),
                ("KEY2".to_owned(), "VAL2".to_owned()),
            ])
            .with_force(true);
        assert!(goal.force);
        let encoded = goal.encode().expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");

        assert_eq!(decoded.source, NodeSource::Fs(PathBuf::from("/some/node")));
        assert_eq!(decoded.git_hash, "git-hash");
        assert_eq!(decoded.timeout_secs, 42);
        assert!(decoded.force);
        assert_eq!(
            decoded.env_vars,
            vec![
                ("KEY1".to_owned(), "VAL1".to_owned()),
                ("KEY2".to_owned(), "VAL2".to_owned()),
            ]
        );
    }

    #[test]
    fn node_add_goal_fs_path_accessor() {
        let goal = NodeAddGoal::new("/some/node", "git-hash", 30);
        assert_eq!(goal.fs_path(), Some(&PathBuf::from("/some/node")));

        let repo_goal = NodeAddGoal::new_repo_node("camera", "v1", "hash", 30)
            .expect("repo_node constructor should accept valid inputs");
        assert_eq!(repo_goal.fs_path(), None);
    }

    // --- NodeAddGoalResponse ---

    #[test]
    fn node_add_goal_response_accepted_roundtrip() {
        let response = NodeAddGoalResponse::accepted("/var/log/add.log");
        assert!(response.accepted);
        assert_eq!(response.log_path, PathBuf::from("/var/log/add.log"));
        assert_eq!(response.rejection_reason, None);
        let encoded = response.encode().expect("encode");
        let decoded = NodeAddGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_add_goal_response_rejected_roundtrip() {
        let response = NodeAddGoalResponse::rejected("busy");
        assert!(!response.accepted);
        assert_eq!(response.log_path, PathBuf::new());
        assert_eq!(response.rejection_reason, Some("busy".to_owned()));
        let encoded = response.encode().expect("encode");
        let decoded = NodeAddGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_add_goal_response_decode_rejects_malformed_bytes() {
        assert!(NodeAddGoalResponse::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeAddFeedback ---

    #[test]
    fn node_add_feedback_from_stream_roundtrip() {
        let feedback = NodeAddFeedback::from_stream(FeedbackStream::Stdout, "line");
        assert_eq!(feedback.stream, FeedbackStream::Stdout);
        assert_eq!(feedback.line, "line");
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeAddFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
    }

    #[test]
    fn node_add_feedback_stdout_predicates() {
        let feedback = NodeAddFeedback::stdout("out");
        assert!(feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeAddFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stdout());
    }

    #[test]
    fn node_add_feedback_stderr_predicates() {
        let feedback = NodeAddFeedback::stderr("err");
        assert!(!feedback.is_stdout());
        assert!(feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeAddFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stderr());
    }

    #[test]
    fn node_add_feedback_warning_predicates() {
        let feedback = NodeAddFeedback::warning("warn");
        assert!(!feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeAddFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_warning());
    }

    #[test]
    fn node_add_feedback_decode_rejects_malformed_bytes() {
        assert!(NodeAddFeedback::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeAddResult ---

    #[test]
    fn node_add_result_success_roundtrip() {
        let result = NodeAddResult::success("/var/log/add.log", "sensor_node", "v1");
        assert!(result.success);
        assert_eq!(result.log_path, PathBuf::from("/var/log/add.log"));
        assert_eq!(result.error_message, None);
        assert_eq!(result.node_name, Some("sensor_node".to_owned()));
        assert_eq!(result.node_tag, Some("v1".to_owned()));
        let encoded = result.encode().expect("encode");
        let decoded = NodeAddResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_add_result_failure_roundtrip() {
        let result = NodeAddResult::failure("/var/log/add.log", "boom");
        assert!(!result.success);
        assert_eq!(result.log_path, PathBuf::from("/var/log/add.log"));
        assert_eq!(result.error_message, Some("boom".to_owned()));
        assert_eq!(result.node_name, None);
        assert_eq!(result.node_tag, None);
        let encoded = result.encode().expect("encode");
        let decoded = NodeAddResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_add_result_decode_rejects_malformed_bytes() {
        assert!(NodeAddResult::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
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
