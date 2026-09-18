//! Encoding types for the Launch action (streaming version with feedback).

use std::collections::BTreeMap;
use std::path::PathBuf;

use capnp::message::Builder;

use crate::launch_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use crate::encoding::endpoints::{read_launch_endpoints, write_launch_endpoints};
use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
    required_text, write_text_list,
};
use crate::graph::InstanceEndpoint;

use super::budgets::StackBudgets;
use super::read_selections;

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

/// How a launch places the core node links its launcher declares.
///
/// The INTENT, not its expansion. Only the coordinator holds the resolved
/// launcher — a [`LauncherOrigin::Repository`] is read from the daemon's own
/// cache, which the caller may never have seen — so the caller cannot know
/// what the document declares. Sending what the user asked for, and expanding
/// it where the document lives, is what makes `--local` mean the same thing
/// for both origins instead of working only for the one the caller can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementSpec {
    /// `--place <core-node-link>@<core-node>` wirings, keyed by the
    /// placeholder declared in the launcher's `core_nodes` list.
    ///
    /// Empty means no placement flag was given at all, which only a launcher
    /// declaring no `core_nodes` can satisfy: the coordinator refuses a
    /// declared-but-unwired link rather than quietly collapsing it onto itself.
    ///
    /// A `BTreeMap` rather than a list because the caller has already rejected
    /// repeats, so "at most one target per placeholder" is a property of the
    /// type instead of a rule the daemon has to re-check.
    Places(BTreeMap<String, String>),
    /// `--local`: every link the launcher declares runs on the coordinator.
    ///
    /// Carries no map on purpose. Which links exist is a property of the
    /// document, so naming them here would mean the caller had to have read it.
    Local,
}

impl Default for PlacementSpec {
    fn default() -> Self {
        Self::Places(BTreeMap::new())
    }
}

/// Goal message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoal {
    pub launcher_origin: LauncherOrigin,
    pub budgets: StackBudgets,
    /// Identity of this launch, minted by the caller. Every participant
    /// records it with its slice, so the global stack is rediscoverable by
    /// query from any machine rather than remembered in a coordinator's RAM.
    ///
    /// Never empty; [`LaunchGoal::decode`] refuses an empty one.
    pub launch_id: String,
    /// What the caller asked for about placement, for the coordinator to
    /// expand against the document it resolved.
    pub placement: PlacementSpec,
    /// The launcher's `--with` selections, verbatim: each entry is `option`
    /// or `axis=option`, exactly as the caller typed it. The coordinator
    /// resolves the words against the document it read, for the same reason
    /// placement travels as intent.
    pub selections: Vec<String>,
    /// Build every node of the launch from its staged sources even when
    /// storage already holds an artifact built from byte-identical sources.
    /// Forwarded to each node build the launch performs, on the coordinator
    /// and on every peer.
    pub rebuild: bool,
}

impl LaunchGoal {
    pub fn new(
        launcher_origin: LauncherOrigin,
        launch_id: impl Into<String>,
        budgets: StackBudgets,
    ) -> Self {
        Self {
            launcher_origin,
            budgets,
            launch_id: launch_id.into(),
            placement: PlacementSpec::default(),
            selections: Vec::new(),
            rebuild: false,
        }
    }

    pub fn with_placement(mut self, placement: PlacementSpec) -> Self {
        self.placement = placement;
        self
    }

    pub fn with_selections(mut self, selections: Vec<String>) -> Self {
        self.selections = selections;
        self
    }

    pub fn with_rebuild(mut self, rebuild: bool) -> Self {
        self.rebuild = rebuild;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<launch_capnp::launch_goal::Builder>();

            let env_var_count = capnp_list_len(self.budgets.env_vars.len(), "LaunchGoal.env_vars")?;
            self.budgets
                .write_env_vars(goal.reborrow().init_env_vars(env_var_count));
            goal.reborrow()
                .set_node_add_idle_timeout_secs(self.budgets.node_add_idle_timeout_secs);
            goal.reborrow()
                .set_node_build_idle_timeout_secs(self.budgets.node_build_idle_timeout_secs);
            goal.reborrow()
                .set_node_run_idle_timeout_secs(self.budgets.node_run_idle_timeout_secs);
            goal.reborrow()
                .set_max_timeout_secs(self.budgets.max_timeout_secs.unwrap_or(0));

            goal.reborrow().set_launch_id(&self.launch_id);

            match &self.placement {
                PlacementSpec::Places(places) => {
                    let link_count = capnp_list_len(places.len(), "LaunchGoal.placement.places")?;
                    let mut links = goal.reborrow().init_placement().init_places(link_count);
                    for (idx, (link_id, core_node)) in places.iter().enumerate() {
                        let mut link = links.reborrow().get(idx as u32);
                        link.set_link_id(link_id);
                        link.set_core_node(core_node);
                    }
                }
                PlacementSpec::Local => goal.reborrow().init_placement().set_local(()),
            }

            let mut origin = goal.reborrow().init_launcher_origin();
            match &self.launcher_origin {
                LauncherOrigin::Fs(path) => {
                    origin.set_fs(path.to_string_lossy().as_ref());
                }
                LauncherOrigin::Repository { name } => {
                    origin.set_repository(name.as_str());
                }
            }

            let selection_count = capnp_list_len(self.selections.len(), "LaunchGoal.selections")?;
            write_text_list(
                goal.reborrow().init_selections(selection_count),
                &self.selections,
            );

            goal.reborrow().set_rebuild(self.rebuild);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use launch_capnp::launch_goal::launcher_origin::Which;
        use launch_capnp::launch_goal::placement::Which as PlacementWhich;

        let reader = decode_message(data)?;
        let goal = reader.get_root::<launch_capnp::launch_goal::Reader>()?;

        let budgets = StackBudgets::decode(
            goal.get_env_vars()?,
            goal.get_node_add_idle_timeout_secs(),
            goal.get_node_build_idle_timeout_secs(),
            goal.get_node_run_idle_timeout_secs(),
            goal.get_max_timeout_secs(),
        )?;

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

        // Cap'n Proto defaults an absent field to the empty string, so a goal
        // from a peppy that predates federated launch decodes here with no
        // launch id and no placements. Left unchecked it would run every
        // instance on this daemon and record an unidentifiable slice, which is
        // exactly the silent-wrong outcome a breaking change must not produce.
        // There is no compatibility path: refuse, and name the fix.
        let launch_id = goal.get_launch_id()?.to_str()?;
        if launch_id.is_empty() {
            return Err(crate::Error::Decoding(
                "LaunchGoal.launch_id is empty: this launch came from a peppy that predates \
                 federated launch. Upgrade the caller to the same version as this daemon."
                    .to_owned(),
            ));
        }

        let placement = match goal.get_placement().which()? {
            PlacementWhich::Local(()) => PlacementSpec::Local,
            PlacementWhich::Places(places) => {
                let places_reader = places?;
                let mut places = BTreeMap::new();
                for idx in 0..places_reader.len() {
                    let link = places_reader.get(idx);
                    let link_id = link.get_link_id()?.to_str()?;
                    let core_node = link.get_core_node()?.to_str()?;
                    if link_id.is_empty() || core_node.is_empty() {
                        return Err(crate::Error::Decoding(format!(
                            "LaunchGoal.placement.places[{idx}] has an empty link_id or core_node"
                        )));
                    }
                    if places
                        .insert(link_id.to_owned(), core_node.to_owned())
                        .is_some()
                    {
                        return Err(crate::Error::Decoding(format!(
                            "LaunchGoal.placement.places wires `{link_id}` more than once"
                        )));
                    }
                }
                PlacementSpec::Places(places)
            }
        };

        let selections = read_selections(goal.get_selections()?, "LaunchGoal.selections")?;

        Ok(Self {
            launch_id: launch_id.to_owned(),
            placement,
            launcher_origin,
            budgets,
            selections,
            rebuild: goal.get_rebuild(),
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
    /// Core node whose filesystem holds the log file.
    pub core_node: String,
}

/// Per-node build log entry carried in `LaunchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildLogEntry {
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
    /// Core node whose filesystem holds the log file.
    pub core_node: String,
}

/// Per-node start log entry carried in `LaunchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRunLogEntry {
    pub instance_id: String,
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
    /// Core node whose filesystem holds the log file.
    pub core_node: String,
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
    /// The endpoints of every started instance that serves one, in start
    /// order, each stamped with the core node hosting it.
    pub instance_endpoints: Vec<InstanceEndpoints>,
}

/// The endpoints of one started instance, as the daemon hosting it expanded
/// them against its own interfaces; a coordinator forwards them with that
/// daemon's core node name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceEndpoints {
    pub instance_id: String,
    /// Node label in "name:tag" format.
    pub node_label: String,
    /// Core node hosting the instance.
    pub core_node: String,
    pub endpoints: Vec<InstanceEndpoint>,
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
            instance_endpoints: Vec::new(),
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

    /// The endpoints of the instances the operation started.
    pub fn with_instance_endpoints(mut self, instance_endpoints: Vec<InstanceEndpoints>) -> Self {
        self.instance_endpoints = instance_endpoints;
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
                e.set_core_node(&entry.core_node);
            }

            let build_log_count =
                capnp_list_len(self.node_build_logs.len(), "LaunchResult.node_build_logs")?;
            let mut build_logs = result.reborrow().init_node_build_logs(build_log_count);
            for (i, entry) in self.node_build_logs.iter().enumerate() {
                let mut e = build_logs.reborrow().get(i as u32);
                e.set_node_label(&entry.node_label);
                e.set_log_path(entry.log_path.to_string_lossy().as_ref());
                e.set_failed(entry.failed);
                e.set_core_node(&entry.core_node);
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
                e.set_core_node(&entry.core_node);
            }

            let instance_count = capnp_list_len(
                self.instance_endpoints.len(),
                "LaunchResult.instance_endpoints",
            )?;
            let mut instances = result.reborrow().init_instance_endpoints(instance_count);
            for (i, entry) in self.instance_endpoints.iter().enumerate() {
                let mut e = instances.reborrow().get(i as u32);
                e.set_instance_id(&entry.instance_id);
                e.set_node_label(&entry.node_label);
                e.set_core_node(&entry.core_node);
                let endpoint_count =
                    capnp_list_len(entry.endpoints.len(), "InstanceEndpoints.endpoints")?;
                write_launch_endpoints(e.init_endpoints(endpoint_count), &entry.endpoints)?;
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
                core_node: e.get_core_node()?.to_str()?.to_owned(),
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
                core_node: e.get_core_node()?.to_str()?.to_owned(),
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
                core_node: e.get_core_node()?.to_str()?.to_owned(),
            });
        }

        let instances_reader = result.get_instance_endpoints()?;
        let mut instance_endpoints = Vec::with_capacity(instances_reader.len() as usize);
        for i in 0..instances_reader.len() {
            let e = instances_reader.get(i);
            instance_endpoints.push(InstanceEndpoints {
                instance_id: required_text(
                    e.get_instance_id()?.to_str()?,
                    "InstanceEndpoints.instance_id",
                )?,
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                core_node: e.get_core_node()?.to_str()?.to_owned(),
                endpoints: read_launch_endpoints(e.get_endpoints()?)?,
            });
        }

        Ok(Self {
            success: result.get_success(),
            log_path,
            error_message,
            node_add_logs,
            node_build_logs,
            node_run_logs,
            instance_endpoints,
        })
    }
}

impl crate::encoding::Wire for LaunchGoal {
    type Root = crate::launch_capnp::launch_goal::Owned;
}

impl crate::encoding::Wire for LaunchGoalResponse {
    type Root = crate::launch_capnp::launch_goal_response::Owned;
}

impl crate::encoding::Wire for LaunchFeedback {
    type Root = crate::launch_capnp::launch_feedback::Owned;
}

impl crate::encoding::Wire for LaunchResult {
    type Root = crate::launch_capnp::launch_result::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;

    #[test]
    fn launch_goal_roundtrips_fs_origin() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs(PathBuf::from("/tmp/launcher.json5")),
            "launch-abc123",
            StackBudgets::new(10, 20, 30, Some(99))
                .with_env_vars(vec![("PATH".to_string(), "/usr/bin".to_string())]),
        );

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
            "launch-abc123",
            StackBudgets::new(5, 10, 15, None),
        );

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(goal, decoded);
        assert_eq!(decoded.budgets.max_timeout_secs, None);
        assert!(!decoded.rebuild);
    }

    /// `--rebuild` travels with the launch so every node build it performs,
    /// local or on a peer, ignores a cached artifact.
    #[test]
    fn launch_goal_roundtrips_rebuild() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "openarm_v2".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
        .with_rebuild(true);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert!(decoded.rebuild);
        assert_eq!(goal, decoded);
    }

    #[test]
    fn launch_goal_roundtrips_core_node_links() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "split_compute_manipulation".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(5, 10, 15, None),
        )
        .with_placement(PlacementSpec::Places(BTreeMap::from([
            ("robot_onboard".to_string(), "cn-robot-7".to_string()),
            ("cloud_inference".to_string(), "cn-atlas-h100".to_string()),
        ])));

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(goal, decoded);
        let PlacementSpec::Places(places) = &decoded.placement else {
            panic!("expected explicit places, got {:?}", decoded.placement);
        };
        assert_eq!(
            places.get("cloud_inference").map(String::as_str),
            Some("cn-atlas-h100")
        );
    }

    /// `--local` travels as INTENT, not as an expansion. The caller cannot
    /// know what a `Repository` launcher declares, so a map here would mean it
    /// had read a document only the coordinator holds.
    #[test]
    fn launch_goal_roundtrips_local_placement() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "split_compute_manipulation".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
        .with_placement(PlacementSpec::Local);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.placement, PlacementSpec::Local);
        assert_eq!(goal, decoded);
    }

    /// A goal that sets no placement decodes as empty `places`, which is what
    /// "neither `--place` nor `--local`" has always meant.
    #[test]
    fn launch_goal_without_placement_decodes_as_no_places() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "teleop".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        );
        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.placement, PlacementSpec::Places(BTreeMap::new()));
    }

    /// The `--with` words travel verbatim: bare option names and
    /// `axis=option` entries alike reach the coordinator exactly as typed,
    /// because only the coordinator holds the document that gives them
    /// meaning.
    #[test]
    fn launch_goal_roundtrips_selections_verbatim() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "openarm_v2".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
        .with_selections(vec![
            "mujoco".to_string(),
            "robot=mujoco".to_string(),
            "xr_commander".to_string(),
        ]);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(
            decoded.selections,
            vec![
                "mujoco".to_string(),
                "robot=mujoco".to_string(),
                "xr_commander".to_string()
            ]
        );
    }

    /// A goal that names no selection decodes with an empty list, which is
    /// what "no `--with` given" means: a composed launcher fills its defaults,
    /// a flat one has nothing to select.
    #[test]
    fn launch_goal_without_selections_decodes_as_empty() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs("/abs/launcher.json5".into()),
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        );
        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert!(decoded.selections.is_empty());
    }

    /// A blank `--with` entry is a caller bug (an empty comma segment), not a
    /// meaningful "select nothing": refuse it at decode so it cannot silently
    /// pass as one.
    #[test]
    fn launch_goal_decode_rejects_an_empty_selection_entry() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs("/abs/launcher.json5".into()),
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
        .with_selections(vec!["mujoco".to_string(), String::new()]);

        let refusal = decoding_error(LaunchGoal::decode(&goal.encode().expect("encode")));
        assert!(
            refusal.contains("`LaunchGoal.selections[1]` is empty"),
            "{refusal}"
        );
        assert!(refusal.contains("`axis=option`"), "{refusal}");
        assert!(refusal.contains("stray comma"), "{refusal}");
    }

    /// Wiring two placeholders to the SAME machine is legitimate: a launcher
    /// that describes a two-machine topology must still run on one box.
    #[test]
    fn launch_goal_roundtrips_two_links_wired_to_one_core_node() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "split_compute_manipulation".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
        .with_placement(PlacementSpec::Places(BTreeMap::from([
            ("robot_onboard".to_string(), "cn-solo".to_string()),
            ("cloud_inference".to_string(), "cn-solo".to_string()),
        ])));

        let bytes = goal.encode().expect("encode");
        assert_eq!(LaunchGoal::decode(&bytes).expect("decode"), goal);
    }

    /// The break with pre-federation callers is enforced HERE, not by the
    /// codec. Cap'n Proto defaults an absent field to the empty string, so an
    /// older caller's goal decodes cleanly unless this guard refuses it, and
    /// the launch would silently run every instance on the coordinator under
    /// an unidentifiable slice.
    #[test]
    fn launch_goal_decode_rejects_empty_launch_id() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "teleop".to_string(),
            },
            "",
            StackBudgets::new(1, 1, 1, None),
        );
        let msg = decoding_error(LaunchGoal::decode(&goal.encode().expect("encode")));
        assert!(msg.contains("launch_id"), "got: {msg}");
        assert!(msg.contains("predates federated launch"), "got: {msg}");
    }

    #[test]
    fn launch_goal_decode_rejects_empty_core_node_link_fields() {
        for (link_id, core_node) in [("", "cn-atlas"), ("cloud_inference", "")] {
            let goal = LaunchGoal::new(
                LauncherOrigin::Repository {
                    name: "teleop".to_string(),
                },
                "launch-abc123",
                StackBudgets::new(1, 1, 1, None),
            )
            .with_placement(PlacementSpec::Places(BTreeMap::from([(
                link_id.to_string(),
                core_node.to_string(),
            )])));
            let msg = decoding_error(LaunchGoal::decode(&goal.encode().expect("encode")));
            assert!(msg.contains("empty link_id or core_node"), "got: {msg}");
        }
    }

    /// `LauncherOrigin::Fs` is documented to carry an absolute path; the
    /// daemon opens it directly without resolving. A relative path here
    /// would silently anchor at the daemon's CWD, which is a footgun.
    #[test]
    fn launch_goal_decode_rejects_relative_fs_path() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs(PathBuf::from("relative/launcher.json5")),
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        );
        let msg = decoding_error(LaunchGoal::decode(&goal.encode().expect("encode")));
        assert!(msg.contains("absolute"), "got: {msg}");
    }

    #[test]
    fn launch_goal_decode_rejects_empty_repository_name() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Repository {
                name: "".to_string(),
            },
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        );
        let msg = decoding_error(LaunchGoal::decode(&goal.encode().expect("encode")));
        assert!(msg.contains("repository name is empty"), "got: {msg}");
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
                    core_node: "cn-coordinator".to_string(),
                }],
                vec![NodeBuildLogEntry {
                    node_label: "planner:v2".to_string(),
                    log_path: PathBuf::from("/var/log/build/planner.log"),
                    failed: true,
                    core_node: "cn-gpu-box".to_string(),
                }],
                vec![NodeRunLogEntry {
                    instance_id: "inst-001".to_string(),
                    node_label: "driver:v3".to_string(),
                    log_path: PathBuf::from("/var/log/run/driver.log"),
                    failed: false,
                    core_node: "cn-robot-arm".to_string(),
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
        assert_eq!(decoded.node_add_logs[0].core_node, "cn-coordinator");
        assert_eq!(decoded.node_build_logs[0].core_node, "cn-gpu-box");
        assert_eq!(decoded.node_run_logs[0].core_node, "cn-robot-arm");
    }

    #[test]
    fn launch_result_decode_rejects_malformed() {
        assert!(LaunchResult::decode(b"not capnp").is_err());
    }

    /// The endpoints of every started instance ride the result with the core
    /// node that expanded them; a launch that started nothing serving
    /// carries the empty list.
    #[test]
    fn launch_result_roundtrips_instance_endpoints() {
        let result = LaunchResult::success("/var/log/launch.log").with_instance_endpoints(vec![
            InstanceEndpoints {
                instance_id: "panel_inst".to_string(),
                node_label: "openarm_web_commander:v1".to_string(),
                core_node: "cn-robot-arm".to_string(),
                endpoints: crate::encoding::endpoints::sample_endpoints(),
            },
            InstanceEndpoints {
                instance_id: "sim_inst".to_string(),
                node_label: "waldo:v1".to_string(),
                core_node: "cn-gpu-box".to_string(),
                endpoints: vec![InstanceEndpoint {
                    label: "viewer".to_string(),
                    kind: config::node::EndpointKind::Page,
                    urls: vec!["https://127.0.0.1:8080/".to_string()],
                }],
            },
        ]);
        let bytes = result.encode().expect("encode");
        let decoded = LaunchResult::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, result);
        assert_eq!(decoded.instance_endpoints[1].core_node, "cn-gpu-box");

        let empty = LaunchResult::success("/var/log/launch.log");
        let decoded =
            LaunchResult::decode(empty.encode().expect("encode").as_ref()).expect("decode");
        assert!(decoded.instance_endpoints.is_empty());
    }
}
