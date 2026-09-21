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
    /// JSON5-encoded pin for the root node of a pinned batch: the exact
    /// manifest bytes to run, identified by content fingerprint, plus the
    /// git location any machine can fetch them from. The daemon
    /// materializes exactly these bytes and never resolves the name
    /// against its own cache. Opaque text here: the pin model lives in
    /// peppy, and this crate has no business re-deriving it.
    Pinned {
        pin_json5: String,
    },
    /// Resolve a `(name, tag)` against the RECEIVING daemon's own repo
    /// cache, then continue exactly as a pinned batch: that daemon
    /// resolves the transitive closure once, pins every entry, and adds
    /// by pin. The entry arm for `peppy node add <name>:<tag>` — the
    /// receiving daemon is the coordinator of its own add.
    ResolveRef {
        name: String,
        tag: String,
    },
    /// The pins of the `mcp_exposure` documents the built-in MCP server
    /// serves, one JSON5 pin per exposure, encoded the way
    /// [`NodeAddGoal::pins_json5`] carries the contract pins they reference.
    /// The daemon materializes exactly these bytes, derives the server's
    /// manifest from them, and registers the built-in node ready to start:
    /// nothing is fetched for a node, generated or built. Opaque text here
    /// for the same reason [`NodeSource::Pinned`] is.
    Exposures {
        pins_json5: Vec<String>,
    },
}

impl NodeSource {
    /// Validated convenience constructor for a `ResolveRef`. Applies the
    /// same name/tag validation as [`Self::decode_resolve_ref`] so callers
    /// cannot build an unsafe source that would later be rejected on the
    /// wire.
    pub fn resolve_ref(name: impl AsRef<str>, tag: impl AsRef<str>) -> Result<Self> {
        Self::decode_resolve_ref(name.as_ref(), tag.as_ref())
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
        match url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(crate::Error::Decoding(format!(
                    "invalid HTTP URL: scheme must be http or https, got `{scheme}`"
                )));
            }
        }
        Ok(Self::Http {
            url,
            sha256: Self::normalize_http_sha256(sha256),
        })
    }

    pub fn decode_resolve_ref(name: &str, tag: &str) -> Result<Self> {
        validate_repo_node_name(name, "resolve-ref name")?;
        validate_repo_node_tag(tag, "resolve-ref tag")?;
        Ok(Self::ResolveRef {
            name: name.to_owned(),
            tag: tag.to_owned(),
        })
    }

    /// The pin is opaque to this crate, but an empty one names nothing and
    /// is refused at decode rather than handed to a daemon to trip over.
    pub fn decode_pinned(pin_json5: &str) -> Result<Self> {
        Ok(Self::Pinned {
            pin_json5: non_empty_pin(pin_json5, "NodeSource.pinned")?,
        })
    }

    /// One pin per exposure; an empty list, or an empty pin in it, names
    /// nothing and is refused the way an empty [`Self::Pinned`] is.
    pub fn decode_exposures(pins_json5: Vec<String>) -> Result<Self> {
        if pins_json5.is_empty() {
            return Err(crate::Error::Decoding(
                "NodeSource.exposures must not be empty".to_owned(),
            ));
        }
        for pin in &pins_json5 {
            non_empty_pin(pin, "NodeSource.exposures")?;
        }
        Ok(Self::Exposures { pins_json5 })
    }
}

fn non_empty_pin(pin_json5: &str, label: &str) -> Result<String> {
    if pin_json5.trim().is_empty() {
        return Err(crate::Error::Decoding(format!("{label} must not be empty")));
    }
    Ok(pin_json5.to_owned())
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
    /// The federated launch this goal is one step of, when a coordinator
    /// dispatched it; `None` for anything a user typed. A reserved daemon
    /// refuses node work that does not name the launch holding it.
    pub launch_id: Option<String>,
    /// JSON5-encoded pins for the rest of a pinned batch: every transitive
    /// node dependency of the [`NodeSource::Pinned`] root, and every
    /// contract and pairing document any manifest in the batch names.
    /// Populated exactly when the source is `Pinned` or `Exposures` (for the
    /// latter: the contract pins the exposures reference). The daemon refuses a
    /// batch whose closure names an entry missing from this list rather
    /// than resolving it by name.
    pub pins_json5: Vec<String>,
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
            launch_id: None,
            pins_json5: Vec::new(),
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

    /// Creates a new NodeAddGoal that names a node by `(name, tag)` for the
    /// receiving daemon to resolve and pin. Returns an error when the name
    /// or tag fails the repo-node validation rules.
    pub fn new_resolve_ref(
        name: impl AsRef<str>,
        tag: impl AsRef<str>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        Ok(Self::from_source(
            NodeSource::resolve_ref(name, tag)?,
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

    /// Marks this goal as one step of a federated launch's dispatch, so the
    /// receiving daemon accepts it while reserved for that launch.
    pub fn with_launch_id(mut self, launch_id: impl Into<String>) -> Self {
        self.launch_id = Some(launch_id.into());
        self
    }

    /// Attaches the closure pins of a pinned batch; see
    /// [`Self::pins_json5`].
    pub fn with_pins(mut self, pins_json5: Vec<String>) -> Self {
        self.pins_json5 = pins_json5;
        self
    }

    /// Returns the filesystem path if the source is `Fs`, otherwise `None`.
    pub fn fs_path(&self) -> Option<&PathBuf> {
        match &self.source {
            NodeSource::Fs(path) => Some(path),
            NodeSource::Git { .. }
            | NodeSource::Http { .. }
            | NodeSource::Pinned { .. }
            | NodeSource::ResolveRef { .. }
            | NodeSource::Exposures { .. } => None,
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
                NodeSource::Pinned { pin_json5 } => {
                    source.set_pinned(pin_json5);
                }
                NodeSource::ResolveRef { name, tag } => {
                    let mut resolve_ref = source.init_resolve_ref();
                    resolve_ref.set_name(name);
                    resolve_ref.set_tag(tag);
                }
                NodeSource::Exposures { pins_json5 } => {
                    let count = capnp_list_len(pins_json5.len(), "NodeSource.exposures")?;
                    crate::encoding::write_text_list(source.init_exposures(count), pins_json5);
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
            if let Some(launch_id) = &self.launch_id {
                goal.reborrow().set_launch_id(launch_id);
            }

            let pin_count = capnp_list_len(self.pins_json5.len(), "NodeAddGoal.pins_json5")?;
            crate::encoding::write_text_list(
                goal.reborrow().init_pins_json5(pin_count),
                &self.pins_json5,
            );
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
            Which::Pinned(pin) => NodeSource::decode_pinned(pin?.to_str()?)?,
            Which::ResolveRef(resolve_ref) => {
                let resolve_ref = resolve_ref?;
                NodeSource::decode_resolve_ref(
                    resolve_ref.get_name()?.to_str()?,
                    resolve_ref.get_tag()?.to_str()?,
                )?
            }
            Which::Exposures(pins) => {
                NodeSource::decode_exposures(crate::encoding::read_text_list(pins?)?)?
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
            launch_id: optional_text(goal.get_launch_id()?.to_str()?),
            pins_json5: crate::encoding::read_text_list(goal.get_pins_json5()?)?,
        })
    }
}

impl crate::encoding::Wire for NodeAddGoal {
    type Root = crate::node_capnp::node_add_goal::Owned;
}

impl crate::encoding::Wire for NodeAddGoalResponse {
    type Root = crate::node_capnp::node_add_goal_response::Owned;
}

impl crate::encoding::Wire for NodeAddFeedback {
    type Root = crate::node_capnp::node_add_feedback::Owned;
}

impl crate::encoding::Wire for NodeAddResult {
    type Root = crate::node_capnp::node_add_result::Owned;
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
            launch_id: None,
            pins_json5: vec![],
        };
        let encoded = goal.encode().expect("encoding should succeed");
        let result = NodeAddGoal::decode(&encoded);
        assert!(result.is_err(), "decoding an empty Fs source should fail");
    }

    /// See `node_run_goal_round_trips_the_launch_it_belongs_to`.
    #[test]
    fn node_add_goal_round_trips_the_launch_it_belongs_to() {
        let dispatched = NodeAddGoal::new("/some/path", "hash", 30).with_launch_id("launch-abc123");
        let decoded = NodeAddGoal::decode(&dispatched.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.launch_id.as_deref(), Some("launch-abc123"));

        let typed = NodeAddGoal::new("/some/path", "hash", 30);
        let decoded = NodeAddGoal::decode(&typed.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.launch_id, None);
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
    fn node_add_goal_resolve_ref_source_roundtrips() {
        let encoded = NodeAddGoal::new_resolve_ref("camera", "v1", "hash", 42)
            .expect("resolve_ref constructor should accept valid inputs")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
        assert_eq!(
            decoded.source,
            NodeSource::ResolveRef {
                name: "camera".to_owned(),
                tag: "v1".to_owned(),
            }
        );
        assert!(decoded.pins_json5.is_empty());
    }

    #[test]
    fn node_add_goal_pinned_source_roundtrips_with_closure_pins() {
        let root = r#"{kind:"node",name:"camera",tag:"v1"}"#;
        let goal = NodeAddGoal::from_source(
            NodeSource::decode_pinned(root).expect("non-empty pin"),
            "hash",
            42,
        )
        .with_pins(vec![
            r#"{kind:"node",name:"driver",tag:"v2"}"#.to_owned(),
            r#"{kind:"contract",name:"frames",tag:"v1"}"#.to_owned(),
        ]);
        let decoded = NodeAddGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(
            decoded.source,
            NodeSource::Pinned {
                pin_json5: root.to_owned()
            }
        );
        assert_eq!(decoded.pins_json5, goal.pins_json5);
    }

    #[test]
    fn node_add_goal_exposures_source_roundtrips_with_contract_pins() {
        let exposures = vec![
            r#"{kind:"mcp_exposure",name:"camera_and_recording",tag:"v1"}"#.to_owned(),
            r#"{kind:"mcp_exposure",name:"camera_and_recording",tag:"v2"}"#.to_owned(),
        ];
        let goal = NodeAddGoal::from_source(
            NodeSource::decode_exposures(exposures.clone()).expect("non-empty pins"),
            "hash",
            42,
        )
        .with_pins(vec![
            r#"{kind:"contract",name:"rgb_camera",tag:"v1"}"#.to_owned(),
        ]);
        let decoded = NodeAddGoal::decode(&goal.encode().expect("encode")).expect("decode");
        assert_eq!(
            decoded.source,
            NodeSource::Exposures {
                pins_json5: exposures
            }
        );
        assert_eq!(decoded.pins_json5, goal.pins_json5);
        assert_eq!(decoded.fs_path(), None);
    }

    /// An empty pin names nothing; refusing it at decode keeps the daemon
    /// from tripping over it later with a worse message.
    #[test]
    fn decode_pinned_rejects_empty_pin() {
        assert!(NodeSource::decode_pinned("").is_err());
        assert!(NodeSource::decode_pinned("   ").is_err());
        assert!(NodeSource::decode_exposures(vec![]).is_err());
        assert!(NodeSource::decode_exposures(vec!["   ".to_owned()]).is_err());
    }

    #[test]
    fn resolve_ref_rejects_invalid_name() {
        assert!(NodeSource::resolve_ref("../etc", "1.0").is_err());
        assert!(NodeSource::resolve_ref("bad name", "1.0").is_err());
    }

    #[test]
    fn resolve_ref_rejects_invalid_tag() {
        assert!(NodeSource::resolve_ref("node", "").is_err());
        assert!(NodeSource::resolve_ref("node", "..").is_err());
    }

    #[test]
    fn decode_resolve_ref_rejects_empty_name() {
        assert!(NodeSource::decode_resolve_ref("", "v1").is_err());
    }

    #[test]
    fn decode_resolve_ref_rejects_empty_tag() {
        assert!(NodeSource::decode_resolve_ref("node", "").is_err());
    }

    #[test]
    fn decode_resolve_ref_rejects_path_traversal_in_name() {
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
                NodeSource::decode_resolve_ref(name, "v1").is_err(),
                "name `{name}` should be rejected"
            );
        }
    }

    #[test]
    fn decode_resolve_ref_rejects_path_traversal_in_tag() {
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
                NodeSource::decode_resolve_ref("node", tag).is_err(),
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

        let resolve_goal = NodeAddGoal::new_resolve_ref("camera", "v1", "hash", 30)
            .expect("resolve_ref constructor should accept valid inputs");
        assert_eq!(resolve_goal.fs_path(), None);
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
