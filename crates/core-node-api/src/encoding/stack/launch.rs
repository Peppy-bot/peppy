//! Encoding types for the Launch action (streaming version with feedback).

use std::path::PathBuf;

use capnp::message::Builder;

use crate::launch_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
};

/// Default idle timeout in seconds for the add/build/run phases (used as fallback when 0 is
/// received on the wire — Cap'n Proto defaults unset `UInt64` to 0).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

/// Applies a default value when a timeout field is 0 (Cap'n Proto defaults unset UInt64 to 0).
fn with_timeout_default(value: u64, default: u64) -> u64 {
    if value == 0 { default } else { value }
}

/// Where the launcher file lives.
///
/// `Fs` carries an absolute path that the daemon opens directly. `Repository` carries the
/// launcher *name* (the file stem of a `.json5` file as recorded in `launchers.json5`); the
/// daemon resolves it against the launcher repository cache before opening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherOrigin {
    Fs(PathBuf),
    Repository { name: String },
}

impl LauncherOrigin {
    pub fn fs(path: impl Into<PathBuf>) -> Self {
        Self::Fs(path.into())
    }

    pub fn repository(name: impl Into<String>) -> Self {
        Self::Repository { name: name.into() }
    }
}

/// Goal message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoal {
    pub launcher_origin: LauncherOrigin,
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
        launcher_origin: LauncherOrigin,
        node_add_idle_timeout_secs: u64,
        node_build_idle_timeout_secs: u64,
        node_run_idle_timeout_secs: u64,
        max_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            launcher_origin,
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

            let mut origin = goal.reborrow().init_launcher_origin();
            match &self.launcher_origin {
                LauncherOrigin::Fs(path) => {
                    origin.set_fs(path.to_string_lossy().as_ref());
                }
                LauncherOrigin::Repository { name } => {
                    origin.set_repository(name.as_str());
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use launch_capnp::launch_goal::launcher_origin::Which;

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

        let launcher_origin = match goal.get_launcher_origin().which()? {
            Which::Fs(fs) => LauncherOrigin::Fs(crate::encoding::decode_absolute_fs_path(
                fs?.to_str()?,
                "LaunchGoal.launcher_origin.fs",
            )?),
            Which::Repository(name) => {
                let name = name?.to_str()?;
                if name.is_empty() {
                    return Err(crate::Error::Decoding(
                        "LaunchGoal.launcher_origin.repository name is empty".to_owned(),
                    ));
                }
                LauncherOrigin::Repository {
                    name: name.to_owned(),
                }
            }
        };

        let raw_max = goal.get_max_timeout_secs();
        Ok(Self {
            launcher_origin,
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

    pub fn encode(&self) -> Result<NonEmptyPayload> {
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
        encode_message_non_empty(&builder)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_goal_roundtrips_fs_origin() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs(PathBuf::from("/tmp/launcher.json5")),
            10,
            20,
            30,
            Some(99),
        )
        .with_env_vars(vec![("PATH".to_string(), "/usr/bin".to_string())]);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(goal, decoded);
    }

    #[test]
    fn launch_goal_roundtrips_repository_origin() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "openarm01_sim_teleop".to_string(),
            },
            5,
            10,
            15,
            None,
        );

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(goal, decoded);
        assert_eq!(decoded.max_timeout_secs, None);
    }

    /// `LauncherOrigin::Fs` is documented to carry an absolute path; the
    /// daemon opens it directly without resolving. A relative path here
    /// would silently anchor at the daemon's CWD, which is a footgun.
    #[test]
    fn launch_goal_decode_rejects_relative_fs_path() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs(PathBuf::from("relative/launcher.json5")),
            1,
            1,
            1,
            None,
        );
        let bytes = goal.encode().expect("encode");
        let err = LaunchGoal::decode(&bytes).expect_err("relative path should fail");
        let crate::Error::Decoding(msg) = err else {
            panic!("expected Decoding error, got {err:?}");
        };
        assert!(msg.contains("absolute"), "got: {msg}");
    }

    #[test]
    fn launch_goal_decode_rejects_empty_repository_name() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "".to_string(),
            },
            1,
            1,
            1,
            None,
        );
        let bytes = goal.encode().expect("encode");
        let err = LaunchGoal::decode(&bytes).expect_err("empty name should fail");
        assert!(matches!(err, crate::Error::Decoding(_)));
    }

    #[test]
    fn launch_feedback_step_phase_labels() {
        assert_eq!(LaunchFeedbackStep::LauncherStep.phase_label(), "launch");
        assert_eq!(LaunchFeedbackStep::AddingNode.phase_label(), "add");
        assert_eq!(LaunchFeedbackStep::BuildingNode.phase_label(), "build");
        assert_eq!(LaunchFeedbackStep::RunningNode.phase_label(), "run");
    }

    #[test]
    fn launch_feedback_stdout_roundtrips_all_steps() {
        for step in [
            LaunchFeedbackStep::LauncherStep,
            LaunchFeedbackStep::AddingNode,
            LaunchFeedbackStep::BuildingNode,
            LaunchFeedbackStep::RunningNode,
        ] {
            let fb = LaunchFeedback::stdout("a line of output", step);
            assert!(fb.is_stdout());
            assert!(!fb.is_stderr());
            assert_eq!(fb.step, step);
            let bytes = fb.encode().expect("encode").into_inner();
            let decoded = LaunchFeedback::decode(bytes.as_ref()).expect("decode");
            assert_eq!(decoded, fb);
            assert!(decoded.is_stdout());
        }
    }

    #[test]
    fn launch_feedback_stderr_roundtrips() {
        let fb = LaunchFeedback::stderr("something failed", LaunchFeedbackStep::BuildingNode);
        assert!(fb.is_stderr());
        assert!(!fb.is_stdout());
        let bytes = fb.encode().expect("encode").into_inner();
        let decoded = LaunchFeedback::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, fb);
        assert!(decoded.is_stderr());
        assert_eq!(decoded.line, "something failed");
    }

    #[test]
    fn launch_feedback_decode_rejects_malformed() {
        assert!(LaunchFeedback::decode(b"not capnp").is_err());
    }

    #[test]
    fn launch_result_new_roundtrips() {
        let result = LaunchResult::new(true, "/var/log/launch.log", None);
        assert!(result.success);
        assert!(result.node_add_logs.is_empty());
        assert!(result.node_build_logs.is_empty());
        assert!(result.node_run_logs.is_empty());
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn launch_result_success_constructor_roundtrips() {
        let result = LaunchResult::success("/var/log/launch.log");
        assert!(result.success);
        assert_eq!(result.error_message, None);
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn launch_result_failure_constructor_roundtrips() {
        let result = LaunchResult::failure("/var/log/launch.log", "node build failed");
        assert!(!result.success);
        assert_eq!(result.error_message.as_deref(), Some("node build failed"));
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
    }

    #[test]
    fn launch_result_with_empty_node_logs_roundtrips() {
        let result = LaunchResult::success("/var/log/launch.log").with_node_logs(
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
        assert!(decoded.node_add_logs.is_empty());
        assert!(decoded.node_build_logs.is_empty());
        assert!(decoded.node_run_logs.is_empty());
    }

    #[test]
    fn launch_result_with_populated_node_logs_roundtrips() {
        let result = LaunchResult::failure("/var/log/launch.log", "one node failed")
            .with_node_logs(
                vec![NodeAddLogEntry {
                    node_label: "camera:v1".to_string(),
                    log_path: PathBuf::from("/var/log/add/camera.log"),
                    failed: false,
                }],
                vec![NodeBuildLogEntry {
                    node_label: "planner:v2".to_string(),
                    log_path: PathBuf::from("/var/log/build/planner.log"),
                    failed: true,
                }],
                vec![NodeRunLogEntry {
                    instance_id: "inst-001".to_string(),
                    node_label: "driver:v3".to_string(),
                    log_path: PathBuf::from("/var/log/run/driver.log"),
                    failed: false,
                }],
            );
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
        assert_eq!(decoded.node_add_logs.len(), 1);
        assert_eq!(decoded.node_build_logs.len(), 1);
        assert_eq!(decoded.node_run_logs.len(), 1);
        assert!(decoded.node_build_logs[0].failed);
        assert_eq!(decoded.node_run_logs[0].instance_id, "inst-001");
    }

    #[test]
    fn launch_result_decode_rejects_malformed() {
        assert!(LaunchResult::decode(b"not capnp").is_err());
    }
}
