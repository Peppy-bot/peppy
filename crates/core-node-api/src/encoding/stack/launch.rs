//! Encoding types for the Launch action (streaming version with feedback).

use std::path::PathBuf;

use capnp::message::Builder;

use crate::launch_capnp;
use crate::{Payload, Result};

use crate::encoding::{capnp_list_len, decode_message, encode_message, optional_text};

/// Default idle timeout in seconds for the add/build/run phases (used as fallback when 0 is
/// received on the wire — Cap'n Proto defaults unset `UInt64` to 0).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

/// Applies a default value when a timeout field is 0 (Cap'n Proto defaults unset UInt64 to 0).
fn with_timeout_default(value: u64, default: u64) -> u64 {
    if value == 0 { default } else { value }
}

/// Goal message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoal {
    pub peppy_launch_file_path: PathBuf,
    pub env_vars: Vec<(String, String)>,
    pub node_add_idle_timeout_secs: u64,
    pub node_build_idle_timeout_secs: u64,
    pub node_run_idle_timeout_secs: u64,
    /// Whole-launch deadline. `None` means no overall deadline is enforced (only idle timeouts
    /// apply). Wire encoding uses 0 as the sentinel for `None`.
    pub max_timeout_secs: Option<u64>,
}

impl LaunchGoal {
    pub fn new(
        peppy_launch_file_path: impl Into<PathBuf>,
        node_add_idle_timeout_secs: u64,
        node_build_idle_timeout_secs: u64,
        node_run_idle_timeout_secs: u64,
        max_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            peppy_launch_file_path: peppy_launch_file_path.into(),
            env_vars: Vec::new(),
            node_add_idle_timeout_secs,
            node_build_idle_timeout_secs,
            node_run_idle_timeout_secs,
            max_timeout_secs,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<launch_capnp::launch_goal::Builder>();
            goal.set_peppy_launch_file_path(self.peppy_launch_file_path.to_string_lossy());

            let env_var_count = capnp_list_len(self.env_vars.len(), "LaunchGoal.env_vars")?;
            let mut env_vars = goal.reborrow().init_env_vars(env_var_count);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow()
                .set_node_add_idle_timeout_secs(self.node_add_idle_timeout_secs);
            goal.reborrow()
                .set_node_build_idle_timeout_secs(self.node_build_idle_timeout_secs);
            goal.reborrow()
                .set_node_run_idle_timeout_secs(self.node_run_idle_timeout_secs);
            // 0 on the wire means "unset" (no overall deadline).
            goal.reborrow()
                .set_max_timeout_secs(self.max_timeout_secs.unwrap_or(0));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<launch_capnp::launch_goal::Reader>()?;

        let env_vars_reader = goal.get_env_vars()?;
        let mut env_vars = Vec::with_capacity(env_vars_reader.len() as usize);
        for idx in 0..env_vars_reader.len() {
            let env_var = env_vars_reader.get(idx);
            env_vars.push((
                env_var.get_key()?.to_str()?.to_owned(),
                env_var.get_value()?.to_str()?.to_owned(),
            ));
        }

        let raw_max = goal.get_max_timeout_secs();
        Ok(Self {
            peppy_launch_file_path: PathBuf::from(goal.get_peppy_launch_file_path()?.to_str()?),
            env_vars,
            node_add_idle_timeout_secs: with_timeout_default(
                goal.get_node_add_idle_timeout_secs(),
                DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            node_build_idle_timeout_secs: with_timeout_default(
                goal.get_node_build_idle_timeout_secs(),
                DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            node_run_idle_timeout_secs: with_timeout_default(
                goal.get_node_run_idle_timeout_secs(),
                DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            max_timeout_secs: if raw_max == 0 { None } else { Some(raw_max) },
        })
    }
}

/// Response to the Launch goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl LaunchGoalResponse {
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
            let mut response = builder.init_root::<launch_capnp::launch_goal_response::Builder>();
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
        let response = reader.get_root::<launch_capnp::launch_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchFeedbackStep {
    LauncherStep,
    AddingNode,
    RunningNode,
    BuildingNode,
}

impl LaunchFeedbackStep {
    /// Short user-facing label for the phase this step represents. Used by timeout error
    /// messages so the surfaced string stays aligned with the feedback stream.
    pub fn phase_label(self) -> &'static str {
        match self {
            Self::LauncherStep => "launch",
            Self::AddingNode => "add",
            Self::BuildingNode => "build",
            Self::RunningNode => "run",
        }
    }
}
/// Feedback message for the Launch action.
/// Represents a single line of output from the launch process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFeedback {
    /// The stream type: "stdout" or "stderr"
    pub stream: String,
    /// The line of output
    pub line: String,
    /// The step in the launch process this feedback is from
    pub step: LaunchFeedbackStep,
}

impl LaunchFeedback {
    pub fn stdout(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self {
            stream: "stdout".to_string(),
            line: line.into(),
            step,
        }
    }

    pub fn stderr(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self {
            stream: "stderr".to_string(),
            line: line.into(),
            step,
        }
    }

    pub fn is_stdout(&self) -> bool {
        self.stream == "stdout"
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == "stderr"
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<launch_capnp::launch_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
            feedback.set_step(match self.step {
                LaunchFeedbackStep::LauncherStep => launch_capnp::LaunchFeedbackStep::LauncherStep,
                LaunchFeedbackStep::AddingNode => launch_capnp::LaunchFeedbackStep::AddingNode,
                LaunchFeedbackStep::RunningNode => launch_capnp::LaunchFeedbackStep::RunningNode,
                LaunchFeedbackStep::BuildingNode => launch_capnp::LaunchFeedbackStep::BuildingNode,
            });
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<launch_capnp::launch_feedback::Reader>()?;
        let step = match feedback.get_step()? {
            launch_capnp::LaunchFeedbackStep::LauncherStep => LaunchFeedbackStep::LauncherStep,
            launch_capnp::LaunchFeedbackStep::AddingNode => LaunchFeedbackStep::AddingNode,
            launch_capnp::LaunchFeedbackStep::RunningNode => LaunchFeedbackStep::RunningNode,
            launch_capnp::LaunchFeedbackStep::BuildingNode => LaunchFeedbackStep::BuildingNode,
        };
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
            step,
        })
    }
}

/// Per-node add log entry carried in `LaunchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddLogEntry {
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
}

/// Per-node build log entry carried in `LaunchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildLogEntry {
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
}

/// Per-node start log entry carried in `LaunchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunLogEntry {
    pub instance_id: String,
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
}

/// Result message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResult {
    pub success: bool,
    pub log_path: PathBuf,
    pub error_message: Option<String>,
    pub node_add_logs: Vec<NodeAddLogEntry>,
    pub node_build_logs: Vec<NodeBuildLogEntry>,
    pub node_run_logs: Vec<NodeRunLogEntry>,
}

impl LaunchResult {
    pub fn new(success: bool, log_path: impl Into<PathBuf>, error_message: Option<String>) -> Self {
        Self {
            success,
            log_path: log_path.into(),
            error_message,
            node_add_logs: Vec::new(),
            node_build_logs: Vec::new(),
            node_run_logs: Vec::new(),
        }
    }

    pub fn success(log_path: impl Into<PathBuf>) -> Self {
        Self::new(true, log_path, None)
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self::new(false, log_path, Some(error_message.into()))
    }

    pub fn with_node_logs(
        mut self,
        add_logs: Vec<NodeAddLogEntry>,
        build_logs: Vec<NodeBuildLogEntry>,
        run_logs: Vec<NodeRunLogEntry>,
    ) -> Self {
        self.node_add_logs = add_logs;
        self.node_build_logs = build_logs;
        self.node_run_logs = run_logs;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<launch_capnp::launch_result::Builder>();
            result.set_success(self.success);
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }

            let add_log_count =
                capnp_list_len(self.node_add_logs.len(), "LaunchResult.node_add_logs")?;
            let mut add_logs = result.reborrow().init_node_add_logs(add_log_count);
            for (i, entry) in self.node_add_logs.iter().enumerate() {
                let mut e = add_logs.reborrow().get(i as u32);
                e.set_node_label(&entry.node_label);
                e.set_log_path(entry.log_path.to_string_lossy().as_ref());
                e.set_failed(entry.failed);
            }

            let build_log_count =
                capnp_list_len(self.node_build_logs.len(), "LaunchResult.node_build_logs")?;
            let mut build_logs = result.reborrow().init_node_build_logs(build_log_count);
            for (i, entry) in self.node_build_logs.iter().enumerate() {
                let mut e = build_logs.reborrow().get(i as u32);
                e.set_node_label(&entry.node_label);
                e.set_log_path(entry.log_path.to_string_lossy().as_ref());
                e.set_failed(entry.failed);
            }

            let run_log_count =
                capnp_list_len(self.node_run_logs.len(), "LaunchResult.node_run_logs")?;
            let mut run_logs = result.reborrow().init_node_run_logs(run_log_count);
            for (i, entry) in self.node_run_logs.iter().enumerate() {
                let mut e = run_logs.reborrow().get(i as u32);
                e.set_instance_id(&entry.instance_id);
                e.set_node_label(&entry.node_label);
                e.set_log_path(entry.log_path.to_string_lossy().as_ref());
                e.set_failed(entry.failed);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<launch_capnp::launch_result::Reader>()?;
        let error_message = optional_text(result.get_error_message()?.to_str()?);
        let log_path = PathBuf::from(result.get_log_path()?.to_str()?);

        let add_logs_reader = result.get_node_add_logs()?;
        let mut node_add_logs = Vec::with_capacity(add_logs_reader.len() as usize);
        for i in 0..add_logs_reader.len() {
            let e = add_logs_reader.get(i);
            node_add_logs.push(NodeAddLogEntry {
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
            });
        }

        let build_logs_reader = result.get_node_build_logs()?;
        let mut node_build_logs = Vec::with_capacity(build_logs_reader.len() as usize);
        for i in 0..build_logs_reader.len() {
            let e = build_logs_reader.get(i);
            node_build_logs.push(NodeBuildLogEntry {
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
            });
        }

        let run_logs_reader = result.get_node_run_logs()?;
        let mut node_run_logs = Vec::with_capacity(run_logs_reader.len() as usize);
        for i in 0..run_logs_reader.len() {
            let e = run_logs_reader.get(i);
            node_run_logs.push(NodeRunLogEntry {
                instance_id: e.get_instance_id()?.to_str()?.to_owned(),
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
            });
        }

        Ok(Self {
            success: result.get_success(),
            log_path,
            error_message,
            node_add_logs,
            node_build_logs,
            node_run_logs,
        })
    }
}
