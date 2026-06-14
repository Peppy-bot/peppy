//! Encoding types for the NodeRun action (streaming version with feedback).

use std::path::PathBuf;

use capnp::message::Builder;

use crate::node_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use super::builder::FeedbackStream;
use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
};

/// Goal message for the NodeRun action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunGoal {
    pub runtime_config_json5: String,
    pub node_name: String,
    pub tag: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
}

impl NodeRunGoal {
    pub fn new(
        runtime_config_json5: impl Into<String>,
        node_name: impl Into<String>,
        tag: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            runtime_config_json5: runtime_config_json5.into(),
            node_name: node_name.into(),
            tag: tag.into(),
            env_vars: Vec::new(),
            timeout_secs,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    /// Builds a goal for in-process execution that bypasses the action-loop
    /// gate (see `services::stack::launch::start_node_directly`). The
    /// `timeout_secs` field feeds the gate's busy-reporting and is unread on
    /// this path, so it is zero by construction.
    pub fn for_internal_execution(
        runtime_config_json5: impl Into<String>,
        node_name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self::new(runtime_config_json5, node_name, tag, 0)
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_run_goal::Builder>();
            goal.set_runtime_config_json5(&self.runtime_config_json5);
            goal.set_node_name(&self.node_name);
            goal.set_tag(&self.tag);

            let env_var_count = capnp_list_len(self.env_vars.len(), "NodeRunGoal.env_vars")?;
            let mut env_vars = goal.reborrow().init_env_vars(env_var_count);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_run_goal::Reader>()?;

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
            runtime_config_json5: goal.get_runtime_config_json5()?.to_str()?.to_owned(),
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            tag: goal.get_tag()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
        })
    }
}

/// Response to the NodeRun goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl NodeRunGoalResponse {
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
            let mut response = builder.init_root::<node_capnp::node_run_goal_response::Builder>();
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
        let response = reader.get_root::<node_capnp::node_run_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the NodeRun action.
/// Represents a single line of output from the run_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunFeedback {
    pub stream: FeedbackStream,
    /// The line of output
    pub line: String,
}

impl NodeRunFeedback {
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
            let mut feedback = builder.init_root::<node_capnp::node_run_feedback::Builder>();
            feedback.set_stream(self.stream.to_capnp());
            feedback.set_line(&self.line);
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_run_feedback::Reader>()?;
        Ok(Self {
            stream: FeedbackStream::from_capnp(feedback.get_stream()?),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeRun action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunResult {
    pub success: bool,
    pub error_message: Option<String>,
    /// Process ID of the started node (None if not available or failed).
    pub pid: Option<u32>,
}

impl NodeRunResult {
    pub fn new(success: bool, error_message: Option<String>, pid: Option<u32>) -> Self {
        Self {
            success,
            error_message,
            pid,
        }
    }

    pub fn success(pid: u32) -> Self {
        Self::new(true, None, Some(pid))
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()), None)
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_run_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_pid(self.pid.unwrap_or(0));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_run_result::Reader>()?;
        let error_message = optional_text(result.get_error_message()?.to_str()?);
        let pid_value = result.get_pid();
        let pid = if pid_value == 0 {
            None
        } else {
            Some(pid_value)
        };
        Ok(Self {
            success: result.get_success(),
            error_message,
            pid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- NodeRunGoal ---

    #[test]
    fn node_run_goal_new_has_empty_env_vars() {
        let goal = NodeRunGoal::new("config", "node", "tag", 30);
        assert_eq!(goal.runtime_config_json5, "config");
        assert_eq!(goal.node_name, "node");
        assert_eq!(goal.tag, "tag");
        assert!(goal.env_vars.is_empty());
        assert_eq!(goal.timeout_secs, 30);
    }

    #[test]
    fn node_run_goal_roundtrip_empty_env_vars() {
        let goal = NodeRunGoal::new("config", "node", "tag", 30);
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        assert!(decoded.env_vars.is_empty());
    }

    #[test]
    fn node_run_goal_roundtrip_populated_env_vars() {
        let goal = NodeRunGoal::new("config", "node", "tag", 42).with_env_vars(vec![
            ("KEY1".to_owned(), "VAL1".to_owned()),
            ("KEY2".to_owned(), "VAL2".to_owned()),
        ]);
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
        assert_eq!(
            decoded.env_vars,
            vec![
                ("KEY1".to_owned(), "VAL1".to_owned()),
                ("KEY2".to_owned(), "VAL2".to_owned()),
            ]
        );
    }

    #[test]
    fn node_run_goal_for_internal_execution_has_zero_timeout() {
        let goal = NodeRunGoal::for_internal_execution("config", "node", "tag");
        assert_eq!(goal.timeout_secs, 0);
        assert!(goal.env_vars.is_empty());
        let encoded = goal.encode().expect("encode");
        let decoded = NodeRunGoal::decode(&encoded).expect("decode");
        assert_eq!(decoded, goal);
    }

    #[test]
    fn node_run_goal_decode_rejects_malformed_bytes() {
        assert!(NodeRunGoal::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunGoalResponse ---

    #[test]
    fn node_run_goal_response_accepted_roundtrip() {
        let response = NodeRunGoalResponse::accepted("/var/log/run.log");
        assert!(response.accepted);
        assert_eq!(response.log_path, PathBuf::from("/var/log/run.log"));
        assert_eq!(response.rejection_reason, None);
        let encoded = response.encode().expect("encode");
        let decoded = NodeRunGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_run_goal_response_rejected_roundtrip() {
        let response = NodeRunGoalResponse::rejected("busy");
        assert!(!response.accepted);
        assert_eq!(response.log_path, PathBuf::new());
        assert_eq!(response.rejection_reason, Some("busy".to_owned()));
        let encoded = response.encode().expect("encode");
        let decoded = NodeRunGoalResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn node_run_goal_response_decode_rejects_malformed_bytes() {
        assert!(NodeRunGoalResponse::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunFeedback ---

    #[test]
    fn node_run_feedback_from_stream_roundtrip() {
        let feedback = NodeRunFeedback::from_stream(FeedbackStream::Stdout, "line");
        assert_eq!(feedback.stream, FeedbackStream::Stdout);
        assert_eq!(feedback.line, "line");
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
    }

    #[test]
    fn node_run_feedback_stdout_predicates() {
        let feedback = NodeRunFeedback::stdout("out");
        assert!(feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stdout());
    }

    #[test]
    fn node_run_feedback_stderr_predicates() {
        let feedback = NodeRunFeedback::stderr("err");
        assert!(!feedback.is_stdout());
        assert!(feedback.is_stderr());
        assert!(!feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_stderr());
    }

    #[test]
    fn node_run_feedback_warning_predicates() {
        let feedback = NodeRunFeedback::warning("warn");
        assert!(!feedback.is_stdout());
        assert!(!feedback.is_stderr());
        assert!(feedback.is_warning());
        let encoded = feedback.encode().expect("encode");
        let decoded = NodeRunFeedback::decode(&encoded.into_inner()).expect("decode");
        assert_eq!(decoded, feedback);
        assert!(decoded.is_warning());
    }

    #[test]
    fn node_run_feedback_decode_rejects_malformed_bytes() {
        assert!(NodeRunFeedback::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    // --- NodeRunResult ---

    #[test]
    fn node_run_result_new_roundtrip() {
        let result = NodeRunResult::new(true, Some("warn".to_owned()), Some(7));
        assert!(result.success);
        assert_eq!(result.error_message, Some("warn".to_owned()));
        assert_eq!(result.pid, Some(7));
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_success_roundtrip() {
        let result = NodeRunResult::success(1234);
        assert!(result.success);
        assert_eq!(result.error_message, None);
        assert_eq!(result.pid, Some(1234));
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_failure_roundtrip() {
        let result = NodeRunResult::failure("boom");
        assert!(!result.success);
        assert_eq!(result.error_message, Some("boom".to_owned()));
        assert_eq!(result.pid, None);
        let encoded = result.encode().expect("encode");
        let decoded = NodeRunResult::decode(&encoded).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn node_run_result_decode_rejects_malformed_bytes() {
        assert!(NodeRunResult::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }
}
