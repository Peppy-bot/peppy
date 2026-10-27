//! Encoding types for the Launch action (streaming version with feedback).

use std::collections::BTreeMap;
use std::path::PathBuf;

use capnp::message::Builder;

use crate::launch_capnp;
use crate::{NonEmptyPayload, Payload, Result};

use crate::encoding::endpoints::{read_launch_endpoints, write_launch_endpoints};
use crate::encoding::{
    capnp_list_len, decode_message, encode_fs_path, encode_message, encode_message_non_empty,
    read_name, required_text,
};
use crate::graph::InstanceEndpoint;
use config::runtime::{CoreNodeTarget, Name};

use super::budgets::StackBudgets;
use super::words::{
    CopyReference, LaunchWord, read_copy_reference, read_core_node_target, read_launch_word,
    write_copy_reference, write_core_node_target, write_launch_word,
};

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

    /// The launcher as a listing names it: the name of a launcher of the
    /// repositories, or the path of a launcher file.
    pub fn label(&self) -> String {
        match self {
            Self::Fs(path) => path.display().to_string(),
            Self::Repository { name } => name.clone(),
        }
    }
}

/// How a launch places the core node links its launcher declares and the
/// copies its file lists.
///
/// The INTENT, not its expansion. Only the coordinator holds the resolved
/// launcher: a [`LauncherOrigin::Repository`] is read from the daemon's own
/// cache, which the caller may never have seen, so the caller cannot know
/// what the document declares. Sending what the user asked for, and expanding
/// it where the document lives, is what makes `--local` mean the same thing
/// for both origins instead of working only for the one the caller can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementSpec {
    /// `--place NAME@CORE_NODE` wirings, keyed by a placeholder declared in
    /// the launcher's `core_nodes` list or by the name of a copy the file
    /// lists. `self` travels as [`CoreNodeTarget::Coordinator`].
    ///
    /// Empty means no placement flag was given at all, which only a launcher
    /// declaring no `core_nodes` can satisfy: the coordinator refuses a
    /// declared-but-unwired link rather than quietly collapsing it onto
    /// itself. A copy of the file that no key names runs on the coordinator.
    ///
    /// A `BTreeMap` rather than a list because the caller has already rejected
    /// repeats, so "at most one target per placeholder" is a property of the
    /// type instead of a rule the daemon has to re-check.
    Places(BTreeMap<Name, CoreNodeTarget>),
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

/// Why the placement, the `--with` words and the `--join` copies of a launch
/// do not go together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchGoalError {
    #[error(
        "`--place {name}@...` names the copy `{name}` that `--join {option}:{name}` adds; place \
         a joined copy where you join it: `--join {option}:{name}@CORE_NODE`"
    )]
    PlacedJoinCopy { option: Name, name: Name },
    #[error(
        "`--local` runs the whole launch on this machine, so no `--join` and no `--with` word \
         names a machine; `{word}` names one"
    )]
    LocalWithMachine { word: String },
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
    placement: PlacementSpec,
    /// The launcher's `--with` words. The coordinator resolves them against
    /// the document it read, for the same reason placement travels as
    /// intent.
    selections: Vec<LaunchWord>,
    /// Build every node of the launch from its staged sources even when
    /// storage already holds an artifact built from byte-identical sources.
    /// Forwarded to each node build the launch performs, on the coordinator
    /// and on every peer.
    pub rebuild: bool,
    /// The copies `--join OPTION:NAME@CORE_NODE` starts with the launch, in
    /// the order they were given.
    joins: Vec<CopyReference>,
    /// Resolve, add and build on the coordinator every joinable node: each
    /// node that a join of an option of a `robot_membership` scope of the
    /// launch can bring, so that such a join on the coordinator builds
    /// nothing. `--no-prebuild` clears it, and the launch then resolves and
    /// builds only the nodes it runs.
    pub prebuild: bool,
}

impl LaunchGoal {
    /// A launch with no placement flag, no word and no joined copy.
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
            joins: Vec::new(),
            prebuild: true,
        }
    }

    /// The launch with its placement, its `--with` words and its `--join`
    /// copies, set together because two rules read them together: a
    /// `--place` key names no `--join` copy, which takes its machine from its
    /// own reference, and under `--local` no `--join` and no word names a
    /// machine. The CLI and [`Self::decode`] both build a goal here, so every
    /// caller gets the same refusal.
    pub fn with_layout(
        mut self,
        placement: PlacementSpec,
        selections: Vec<LaunchWord>,
        joins: Vec<CopyReference>,
    ) -> std::result::Result<Self, LaunchGoalError> {
        match &placement {
            PlacementSpec::Places(places) => {
                if let Some(join) = joins
                    .iter()
                    .find(|join| places.contains_key(join.name.as_name()))
                {
                    return Err(LaunchGoalError::PlacedJoinCopy {
                        option: join.option.clone(),
                        name: join.name.as_name().clone(),
                    });
                }
            }
            PlacementSpec::Local => {
                let placed = joins
                    .iter()
                    .find(|join| join.core_node != CoreNodeTarget::Coordinator)
                    .map(ToString::to_string)
                    .or_else(|| {
                        selections
                            .iter()
                            .find(|word| word.core_node().is_some())
                            .map(ToString::to_string)
                    });
                if let Some(word) = placed {
                    return Err(LaunchGoalError::LocalWithMachine { word });
                }
            }
        }
        self.placement = placement;
        self.selections = selections;
        self.joins = joins;
        Ok(self)
    }

    pub fn with_rebuild(mut self, rebuild: bool) -> Self {
        self.rebuild = rebuild;
        self
    }

    pub fn with_prebuild(mut self, prebuild: bool) -> Self {
        self.prebuild = prebuild;
        self
    }

    pub fn placement(&self) -> &PlacementSpec {
        &self.placement
    }

    pub fn selections(&self) -> &[LaunchWord] {
        &self.selections
    }

    pub fn joins(&self) -> &[CopyReference] {
        &self.joins
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
                        link.set_link_id(link_id.as_str());
                        write_core_node_target(link.init_core_node(), core_node);
                    }
                }
                PlacementSpec::Local => goal.reborrow().init_placement().set_local(()),
            }

            let mut origin = goal.reborrow().init_launcher_origin();
            match &self.launcher_origin {
                LauncherOrigin::Fs(path) => {
                    origin.set_fs(encode_fs_path(path, "LaunchGoal.launcher_origin.fs")?);
                }
                LauncherOrigin::Repository { name } => {
                    origin.set_repository(name.as_str());
                }
            }

            let selection_count = capnp_list_len(self.selections.len(), "LaunchGoal.selections")?;
            let mut selections = goal.reborrow().init_selections(selection_count);
            for (idx, word) in self.selections.iter().enumerate() {
                write_launch_word(selections.reborrow().get(idx as u32), word);
            }

            goal.reborrow().set_rebuild(self.rebuild);

            let join_count = capnp_list_len(self.joins.len(), "LaunchGoal.joins")?;
            let mut joins = goal.reborrow().init_joins(join_count);
            for (idx, join) in self.joins.iter().enumerate() {
                write_copy_reference(joins.reborrow().get(idx as u32), join);
            }

            goal.reborrow().set_prebuild(self.prebuild);
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
            PlacementWhich::Places(places) => PlacementSpec::Places(read_places(places?)?),
        };

        let selections = goal
            .get_selections()?
            .iter()
            .enumerate()
            .map(|(index, word)| read_launch_word(word, &format!("LaunchGoal.selections[{index}]")))
            .collect::<Result<Vec<_>>>()?;
        let joins = goal
            .get_joins()?
            .iter()
            .enumerate()
            .map(|(index, join)| read_copy_reference(join, &format!("LaunchGoal.joins[{index}]")))
            .collect::<Result<Vec<_>>>()?;

        LaunchGoal::new(launcher_origin, launch_id, budgets)
            .with_rebuild(goal.get_rebuild())
            .with_prebuild(goal.get_prebuild())
            .with_layout(placement, selections, joins)
            .map_err(|error| crate::Error::Decoding(error.to_string()))
    }
}

/// Reads the `--place` wirings of a [`LaunchGoal`]. A link that is not a
/// name, a machine that is not a target, and a link wired twice are
/// refused, which keeps "at most one target per placeholder" true of every
/// decoded [`PlacementSpec::Places`].
fn read_places(
    places: capnp::struct_list::Reader<'_, launch_capnp::core_node_link::Owned>,
) -> Result<BTreeMap<Name, CoreNodeTarget>> {
    let mut wirings = BTreeMap::new();
    for (index, link) in places.iter().enumerate() {
        let field = format!("LaunchGoal.placement.places[{index}]");
        let link_id = read_name(link.get_link_id()?.to_str()?, &format!("{field}.link_id"))?;
        if !link.has_core_node() {
            return Err(crate::Error::Decoding(format!(
                "`{field}.core_node` is missing"
            )));
        }
        let core_node =
            read_core_node_target(link.get_core_node()?, &format!("{field}.core_node"))?;
        if wirings.insert(link_id.clone(), core_node).is_some() {
            return Err(crate::Error::Decoding(format!(
                "LaunchGoal.placement.places wires `{link_id}` more than once"
            )));
        }
    }
    Ok(wirings)
}

/// Goal message for the Build action: the launch `peppy stack build` runs
/// up to its builds.
///
/// It carries a [`LaunchGoal`] and encodes as one: a build IS a launch that
/// stops once every node is built. Its own action name, `stack_build`, is
/// what a daemon must serve to accept those bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct StackBuildGoal(LaunchGoal);

impl StackBuildGoal {
    /// The launch this build runs up to its builds.
    pub fn launch(&self) -> &LaunchGoal {
        &self.0
    }

    pub fn into_launch(self) -> LaunchGoal {
        self.0
    }

    pub fn encode(&self) -> Result<Payload> {
        self.0.encode()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        LaunchGoal::decode(data).map(Self)
    }
}

impl From<LaunchGoal> for StackBuildGoal {
    fn from(launch: LaunchGoal) -> Self {
        Self(launch)
    }
}

impl crate::encoding::Wire for StackBuildGoal {
    type Root = crate::launch_capnp::launch_goal::Owned;
}

/// The goal response of an accepted stack goal: where the daemon writes the
/// goal's log. The daemon refuses a goal in the goal envelope, with a
/// failure kind and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoalResponse {
    pub log_path: PathBuf,
}

impl LaunchGoalResponse {
    pub fn new(log_path: impl Into<PathBuf>) -> Self {
        Self {
            log_path: log_path.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<launch_capnp::launch_goal_response::Builder>();
            response.set_log_path(self.log_path.to_string_lossy().as_ref());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<launch_capnp::launch_goal_response::Reader>()?;
        Ok(Self {
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
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

    fn to_wire(self) -> launch_capnp::LaunchFeedbackStep {
        match self {
            Self::LauncherStep => launch_capnp::LaunchFeedbackStep::LauncherStep,
            Self::AddingNode => launch_capnp::LaunchFeedbackStep::AddingNode,
            Self::RunningNode => launch_capnp::LaunchFeedbackStep::RunningNode,
            Self::BuildingNode => launch_capnp::LaunchFeedbackStep::BuildingNode,
        }
    }

    fn from_wire(step: launch_capnp::LaunchFeedbackStep) -> Self {
        match step {
            launch_capnp::LaunchFeedbackStep::LauncherStep => Self::LauncherStep,
            launch_capnp::LaunchFeedbackStep::AddingNode => Self::AddingNode,
            launch_capnp::LaunchFeedbackStep::RunningNode => Self::RunningNode,
            launch_capnp::LaunchFeedbackStep::BuildingNode => Self::BuildingNode,
        }
    }
}

/// One line of output of a stack goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOutput {
    /// The stream type: "stdout" or "stderr"
    pub stream: String,
    /// The line of output
    pub line: String,
    /// The step in the launch process this feedback is from
    pub step: LaunchFeedbackStep,
}

impl LaunchOutput {
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
}

/// Feedback message of a stack goal: one line of its output, or the log
/// file of each node phase it ran, which the daemon sends once, as the last
/// feedback of the goal, whichever way the goal ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchFeedback {
    Output(LaunchOutput),
    NodeLogs(NodeLogs),
}

impl LaunchFeedback {
    /// One line of output on stdout.
    pub fn stdout(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self::Output(LaunchOutput::stdout(line, step))
    }

    /// One line of output on stderr.
    pub fn stderr(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self::Output(LaunchOutput::stderr(line, step))
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let feedback = builder.init_root::<launch_capnp::launch_feedback::Builder>();
            match self {
                Self::Output(output) => {
                    let mut wire = feedback.init_output();
                    wire.set_stream(&output.stream);
                    wire.set_line(&output.line);
                    wire.set_step(output.step.to_wire());
                }
                Self::NodeLogs(logs) => logs.write(feedback.init_node_logs())?,
            }
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<launch_capnp::launch_feedback::Reader>()?;
        match feedback.which()? {
            launch_capnp::launch_feedback::Output(output) => Ok(Self::Output(LaunchOutput {
                stream: output.get_stream()?.to_str()?.to_owned(),
                line: output.get_line()?.to_str()?.to_owned(),
                step: LaunchFeedbackStep::from_wire(output.get_step()?),
            })),
            launch_capnp::launch_feedback::NodeLogs(logs) => {
                Ok(Self::NodeLogs(NodeLogs::read(logs?)?))
            }
        }
    }
}

/// The log file of each node phase a stack goal ran, with `failed` set on
/// the phase that failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeLogs {
    pub node_add_logs: Vec<NodeAddLogEntry>,
    pub node_build_logs: Vec<NodeBuildLogEntry>,
    pub node_run_logs: Vec<NodeRunLogEntry>,
}

impl NodeLogs {
    pub fn is_empty(&self) -> bool {
        self.node_add_logs.is_empty()
            && self.node_build_logs.is_empty()
            && self.node_run_logs.is_empty()
    }

    /// The log file of each node phase that failed: add, then build, then
    /// run.
    pub fn failed(&self) -> impl Iterator<Item = &std::path::Path> {
        let add = self
            .node_add_logs
            .iter()
            .filter(|entry| entry.failed)
            .map(|entry| entry.log_path.as_path());
        let build = self
            .node_build_logs
            .iter()
            .filter(|entry| entry.failed)
            .map(|entry| entry.log_path.as_path());
        let run = self
            .node_run_logs
            .iter()
            .filter(|entry| entry.failed)
            .map(|entry| entry.log_path.as_path());
        add.chain(build).chain(run)
    }

    fn write(&self, mut wire: launch_capnp::node_logs::Builder<'_>) -> Result<()> {
        let add_log_count = capnp_list_len(self.node_add_logs.len(), "NodeLogs.node_add_logs")?;
        let mut add_logs = wire.reborrow().init_node_add_logs(add_log_count);
        for (i, entry) in self.node_add_logs.iter().enumerate() {
            let mut e = add_logs.reborrow().get(i as u32);
            e.set_node_label(&entry.node_label);
            e.set_log_path(entry.log_path.to_string_lossy().as_ref());
            e.set_failed(entry.failed);
            e.set_core_node(&entry.core_node);
        }

        let build_log_count =
            capnp_list_len(self.node_build_logs.len(), "NodeLogs.node_build_logs")?;
        let mut build_logs = wire.reborrow().init_node_build_logs(build_log_count);
        for (i, entry) in self.node_build_logs.iter().enumerate() {
            let mut e = build_logs.reborrow().get(i as u32);
            e.set_node_label(&entry.node_label);
            e.set_log_path(entry.log_path.to_string_lossy().as_ref());
            e.set_failed(entry.failed);
            e.set_core_node(&entry.core_node);
        }

        let run_log_count = capnp_list_len(self.node_run_logs.len(), "NodeLogs.node_run_logs")?;
        let mut run_logs = wire.reborrow().init_node_run_logs(run_log_count);
        for (i, entry) in self.node_run_logs.iter().enumerate() {
            let mut e = run_logs.reborrow().get(i as u32);
            e.set_instance_id(&entry.instance_id);
            e.set_node_label(&entry.node_label);
            e.set_log_path(entry.log_path.to_string_lossy().as_ref());
            e.set_failed(entry.failed);
            e.set_core_node(&entry.core_node);
        }
        Ok(())
    }

    fn read(wire: launch_capnp::node_logs::Reader<'_>) -> Result<Self> {
        let add_logs_reader = wire.get_node_add_logs()?;
        let mut node_add_logs = Vec::with_capacity(add_logs_reader.len() as usize);
        for e in add_logs_reader.iter() {
            node_add_logs.push(NodeAddLogEntry {
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
                core_node: e.get_core_node()?.to_str()?.to_owned(),
            });
        }

        let build_logs_reader = wire.get_node_build_logs()?;
        let mut node_build_logs = Vec::with_capacity(build_logs_reader.len() as usize);
        for e in build_logs_reader.iter() {
            node_build_logs.push(NodeBuildLogEntry {
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
                core_node: e.get_core_node()?.to_str()?.to_owned(),
            });
        }

        let run_logs_reader = wire.get_node_run_logs()?;
        let mut node_run_logs = Vec::with_capacity(run_logs_reader.len() as usize);
        for e in run_logs_reader.iter() {
            node_run_logs.push(NodeRunLogEntry {
                instance_id: e.get_instance_id()?.to_str()?.to_owned(),
                node_label: e.get_node_label()?.to_str()?.to_owned(),
                log_path: PathBuf::from(e.get_log_path()?.to_str()?),
                failed: e.get_failed(),
                core_node: e.get_core_node()?.to_str()?.to_owned(),
            });
        }

        Ok(Self {
            node_add_logs,
            node_build_logs,
            node_run_logs,
        })
    }
}

/// Per-node add log entry carried in [`NodeLogs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddLogEntry {
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
    /// Core node whose filesystem holds the log file.
    pub core_node: String,
}

/// Per-node build log entry carried in [`NodeLogs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildLogEntry {
    /// Node label in "name:tag" format.
    pub node_label: String,
    pub log_path: PathBuf,
    pub failed: bool,
    /// Core node whose filesystem holds the log file.
    pub core_node: String,
}

/// Per-node start log entry carried in [`NodeLogs`].
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

/// The result of a completed stack goal. A failed goal gives a failure kind
/// and a message, and no result; a cancelled goal gives its reason, and no
/// result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchResult {
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
    /// The result of a goal whose started instances serve `instance_endpoints`.
    pub fn new(instance_endpoints: Vec<InstanceEndpoints>) -> Self {
        Self { instance_endpoints }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<launch_capnp::launch_result::Builder>();
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
        let instances_reader = result.get_instance_endpoints()?;
        let mut instance_endpoints = Vec::with_capacity(instances_reader.len() as usize);
        for e in instances_reader.iter() {
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
        Ok(Self { instance_endpoints })
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

    fn words(texts: &[&str]) -> Vec<LaunchWord> {
        texts.iter().map(|text| text.parse().unwrap()).collect()
    }

    fn copies(texts: &[&str]) -> Vec<CopyReference> {
        texts.iter().map(|text| text.parse().unwrap()).collect()
    }

    fn places(pairs: &[(&str, &str)]) -> PlacementSpec {
        PlacementSpec::Places(
            pairs
                .iter()
                .map(|(link, machine)| (Name::new(*link).unwrap(), machine.parse().unwrap()))
                .collect(),
        )
    }

    /// A launch of the repository launcher `name` with the smallest budgets.
    fn repository_goal(name: &str) -> LaunchGoal {
        LaunchGoal::new(
            LauncherOrigin::repository(name),
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        )
    }

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
        assert!(
            decoded.prebuild,
            "a launch builds its joinable nodes by default"
        );
    }

    /// `peppy stack build` sends the same bytes under its own action, so a
    /// daemon that does not serve `stack_build` never mistakes one for a
    /// launch.
    #[test]
    fn a_build_goal_carries_the_launch_it_would_run() {
        let goal = repository_goal("openarm_simulation")
            .with_layout(PlacementSpec::default(), words(&["mujoco"]), Vec::new())
            .unwrap();
        let build = StackBuildGoal::from(goal.clone());

        let bytes = build.encode().expect("encode");
        assert_eq!(StackBuildGoal::decode(&bytes).expect("decode"), build);
        assert_eq!(build.into_launch(), goal);
    }

    #[test]
    fn launch_goal_roundtrips_joins_in_order_and_refuses_a_bad_one() {
        let goal = repository_goal("fleet")
            .with_layout(
                PlacementSpec::default(),
                Vec::new(),
                copies(&["openarm_sim:alpha", "so101_sim:charlie@cn-jetson-1"]),
            )
            .unwrap();
        let decoded = LaunchGoal::decode(&goal.encode().unwrap()).unwrap();
        assert_eq!(decoded.joins(), goal.joins());
        assert!(
            LaunchGoal::decode(&repository_goal("fleet").encode().unwrap())
                .unwrap()
                .joins()
                .is_empty()
        );

        for (option, name, expected) in [
            ("", "alpha", "`LaunchGoal.joins[0].option`"),
            ("openarm_sim", "self", "`LaunchGoal.joins[0].name`"),
        ] {
            let mut builder = Builder::new_default();
            {
                let mut wire = launch_wire(&mut builder);
                let mut join = wire.reborrow().init_joins(1).get(0);
                join.set_option(option);
                join.set_name(name);
                join.init_core_node().set_coordinator(());
            }
            let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
            assert!(refusal.contains(expected), "{refusal}");
        }
        let mut builder = Builder::new_default();
        {
            let mut wire = launch_wire(&mut builder);
            let mut join = wire.reborrow().init_joins(1).get(0);
            join.set_option("openarm_sim");
            join.set_name("alpha");
        }
        let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
        assert!(
            refusal.contains("`LaunchGoal.joins[0].core_node` is missing"),
            "{refusal}"
        );
    }

    /// A wire launch of the repository launcher `fleet` with valid budgets,
    /// for the tests that write one field by hand.
    fn launch_wire(
        builder: &mut Builder<capnp::message::HeapAllocator>,
    ) -> launch_capnp::launch_goal::Builder<'_> {
        let mut wire = builder.init_root::<launch_capnp::launch_goal::Builder>();
        wire.set_launch_id("launch-1");
        wire.set_node_add_idle_timeout_secs(1);
        wire.set_node_build_idle_timeout_secs(1);
        wire.set_node_run_idle_timeout_secs(1);
        wire.reborrow()
            .init_launcher_origin()
            .set_repository("fleet");
        wire
    }

    /// A `--place` key that names a `--join` copy, and `--local` with a
    /// machine on a `--join` or a word, are refused by the one constructor,
    /// for the CLI and at decode alike.
    #[test]
    fn the_layout_rules_hold_at_construction_and_at_decode() {
        let refusal = repository_goal("fleet")
            .with_layout(
                places(&[("xiaozhu", "cn-gpu-box")]),
                Vec::new(),
                copies(&["openarm_sim:xiaozhu"]),
            )
            .unwrap_err();
        assert_eq!(
            refusal.to_string(),
            "`--place xiaozhu@...` names the copy `xiaozhu` that `--join openarm_sim:xiaozhu` \
             adds; place a joined copy where you join it: `--join openarm_sim:xiaozhu@CORE_NODE`"
        );
        for (selections, joins, word) in [
            (
                Vec::new(),
                copies(&["openarm_sim:alpha@cn-x"]),
                "openarm_sim:alpha@cn-x",
            ),
            (
                words(&["alpha.ai_brain_vla@self"]),
                copies(&["openarm_sim:alpha"]),
                "alpha.ai_brain_vla@self",
            ),
        ] {
            let refusal = repository_goal("fleet")
                .with_layout(PlacementSpec::Local, selections, joins)
                .unwrap_err();
            assert_eq!(
                refusal,
                LaunchGoalError::LocalWithMachine {
                    word: word.to_owned()
                }
            );
            assert!(
                refusal
                    .to_string()
                    .starts_with("`--local` runs the whole launch on this machine")
            );
        }
        // A copy reference holds the coordinator when it names no machine,
        // so `@self` on a joined copy is the reference with no machine.
        assert!(
            repository_goal("fleet")
                .with_layout(
                    PlacementSpec::Local,
                    words(&["alpha.ai_brain_vla", "mujoco"]),
                    copies(&["openarm_sim:alpha", "so101_sim:bravo@self"]),
                )
                .is_ok()
        );

        let mut builder = Builder::new_default();
        {
            let mut wire = launch_wire(&mut builder);
            wire.reborrow().init_placement().set_local(());
            let mut join = wire.init_joins(1).get(0);
            join.set_option("openarm_sim");
            join.set_name("alpha");
            join.init_core_node().set_named("cn-x");
        }
        let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
        assert!(
            refusal.starts_with("`--local` runs the whole launch"),
            "{refusal}"
        );

        let mut builder = Builder::new_default();
        {
            let mut wire = launch_wire(&mut builder);
            let mut link = wire.reborrow().init_placement().init_places(1).get(0);
            link.set_link_id("alpha");
            link.init_core_node().set_named("cn-x");
            let mut join = wire.init_joins(1).get(0);
            join.set_option("openarm_sim");
            join.set_name("alpha");
            join.init_core_node().set_coordinator(());
        }
        let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
        assert!(
            refusal.contains("place a joined copy where you join it"),
            "{refusal}"
        );
    }

    /// `--rebuild` travels with the launch so every node build it performs,
    /// local or on a peer, ignores a cached artifact.
    #[test]
    fn launch_goal_roundtrips_rebuild() {
        let goal = repository_goal("openarm_v2").with_rebuild(true);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert!(decoded.rebuild);
        assert_eq!(goal, decoded);
    }

    /// `--no-prebuild` travels with the launch, so the coordinator neither
    /// resolves nor builds its joinable nodes.
    #[test]
    fn launch_goal_roundtrips_no_prebuild() {
        let goal = repository_goal("simulation_mcp").with_prebuild(false);

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert!(!decoded.prebuild);
        assert_eq!(goal, decoded);
    }

    #[test]
    fn launch_goal_roundtrips_core_node_links() {
        let goal = repository_goal("split_compute_manipulation")
            .with_layout(
                places(&[
                    ("robot_onboard", "self"),
                    ("cloud_inference", "cn-atlas-h100"),
                ]),
                Vec::new(),
                Vec::new(),
            )
            .unwrap();

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(goal, decoded);
        let PlacementSpec::Places(places) = decoded.placement() else {
            panic!("expected explicit places, got {:?}", decoded.placement());
        };
        assert_eq!(
            places.get(&Name::new("cloud_inference").unwrap()),
            Some(&"cn-atlas-h100".parse().unwrap())
        );
        assert_eq!(
            places.get(&Name::new("robot_onboard").unwrap()),
            Some(&CoreNodeTarget::Coordinator)
        );
    }

    /// `--local` travels as INTENT, not as an expansion. The caller cannot
    /// know what a `Repository` launcher declares, so a map here would mean it
    /// had read a document only the coordinator holds.
    #[test]
    fn launch_goal_roundtrips_local_placement() {
        let goal = repository_goal("split_compute_manipulation")
            .with_layout(PlacementSpec::Local, Vec::new(), Vec::new())
            .unwrap();

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.placement(), &PlacementSpec::Local);
        assert_eq!(goal, decoded);
    }

    /// A goal that sets no placement decodes as empty `places`, which means
    /// neither `--place` nor `--local` was given.
    #[test]
    fn launch_goal_without_placement_decodes_as_no_places() {
        let goal = repository_goal("teleop");
        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.placement(), &PlacementSpec::Places(BTreeMap::new()));
    }

    /// The `--with` words travel as the caller wrote them: bare options,
    /// `axis=option` and the words of a copy with their machines alike,
    /// because only the coordinator holds the document that gives them
    /// meaning.
    #[test]
    fn launch_goal_roundtrips_selections_as_written() {
        let selections = words(&[
            "mujoco",
            "robot=mujoco",
            "xiaozhu.ai_brain_vla@cn-gpu-box",
            "xiaozhu.camera_rig=cameras_sim@self",
            "xiaozhu.xr_commander",
        ]);
        let goal = repository_goal("openarm_v2")
            .with_layout(
                PlacementSpec::default(),
                selections.clone(),
                copies(&["openarm_sim:xiaozhu"]),
            )
            .unwrap();

        let bytes = goal.encode().expect("encode");
        let decoded = LaunchGoal::decode(&bytes).expect("decode");
        assert_eq!(decoded.selections(), selections.as_slice());
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
        assert!(decoded.selections().is_empty());
    }

    /// A word whose option is not a name, or whose stack choice is null, is
    /// refused at decode, naming the entry.
    #[test]
    fn launch_goal_decode_rejects_a_bad_selection_entry() {
        let mut builder = Builder::new_default();
        {
            let mut wire = launch_wire(&mut builder);
            let mut selections = wire.reborrow().init_selections(2);
            let mut choice = selections.reborrow().get(0).init_stack();
            choice.reborrow().init_axis().set_unnamed(());
            choice.set_option("mujoco");
            let mut choice = selections.reborrow().get(1).init_stack();
            choice.reborrow().init_axis().set_unnamed(());
            choice.set_option("");
        }
        let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
        assert!(
            refusal.contains("`LaunchGoal.selections[1].stack.option`"),
            "{refusal}"
        );
    }

    /// Wiring two placeholders to the SAME machine is legitimate: a launcher
    /// that describes a two-machine topology must still run on one box.
    #[test]
    fn launch_goal_roundtrips_two_links_wired_to_one_core_node() {
        let goal = repository_goal("split_compute_manipulation")
            .with_layout(
                places(&[("robot_onboard", "cn-solo"), ("cloud_inference", "cn-solo")]),
                Vec::new(),
                Vec::new(),
            )
            .unwrap();

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
    fn launch_goal_decode_rejects_bad_core_node_link_fields() {
        for (link_id, core_node, expected) in [
            ("", Some("cn-atlas"), "places[0].link_id"),
            ("cloud_inference", Some(""), "places[0].core_node.named"),
            ("cloud_inference", Some("self"), "places[0].core_node.named"),
            (
                "cloud_inference",
                None,
                "`LaunchGoal.placement.places[0].core_node` is missing",
            ),
        ] {
            let mut builder = Builder::new_default();
            {
                let mut link = launch_wire(&mut builder)
                    .init_placement()
                    .init_places(1)
                    .get(0);
                link.set_link_id(link_id);
                if let Some(core_node) = core_node {
                    link.init_core_node().set_named(core_node);
                }
            }
            let msg = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
            assert!(msg.contains(expected), "got: {msg}");
        }
    }

    /// A [`PlacementSpec::Places`] map cannot hold one link twice, so only a
    /// hand-built message reaches this refusal. It is what keeps "at most one
    /// target per placeholder" a property of the decoded type.
    #[test]
    fn launch_goal_decode_refuses_a_link_wired_twice() {
        let mut builder = Builder::new_default();
        {
            let mut places = launch_wire(&mut builder).init_placement().init_places(2);
            for (index, core_node) in ["cn-a", "cn-b"].into_iter().enumerate() {
                let mut link = places.reborrow().get(index as u32);
                link.set_link_id("robot_onboard");
                link.init_core_node().set_named(core_node);
            }
        }
        let refusal = decoding_error(LaunchGoal::decode(&encode_message(&builder).unwrap()));
        assert!(
            refusal.contains("wires `robot_onboard` more than once"),
            "{refusal}"
        );
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

    /// The daemon opens `LauncherOrigin::Fs` as sent, and the wire carries it
    /// as text. A path that is not UTF-8 would reach the daemon with its
    /// invalid bytes replaced, naming a file the caller never pointed at, so
    /// the encoder refuses it.
    #[cfg(unix)]
    #[test]
    fn launch_goal_encode_refuses_a_launcher_path_that_is_not_utf8() {
        let goal = LaunchGoal::new(
            LauncherOrigin::Fs(crate::encoding::non_utf8_path()),
            "launch-abc123",
            StackBudgets::new(1, 1, 1, None),
        );
        let refusal = crate::encoding::encoding_error(goal.encode());
        assert!(
            refusal.contains("LaunchGoal.launcher_origin.fs is not valid UTF-8"),
            "{refusal}"
        );
    }

    #[test]
    fn launch_goal_decode_rejects_empty_repository_name() {
        let goal = repository_goal("");
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
            let output = LaunchOutput::stdout("a line of output", step);
            assert!(output.is_stdout());
            assert!(!output.is_stderr());
            assert_eq!(output.step, step);
            let fb = LaunchFeedback::Output(output);
            let bytes = fb.encode().expect("encode").into_inner();
            let decoded = LaunchFeedback::decode(bytes.as_ref()).expect("decode");
            assert_eq!(decoded, fb);
            assert_eq!(decoded, LaunchFeedback::stdout("a line of output", step));
        }
    }

    #[test]
    fn launch_feedback_stderr_roundtrips() {
        let fb = LaunchFeedback::stderr("something failed", LaunchFeedbackStep::BuildingNode);
        let bytes = fb.encode().expect("encode").into_inner();
        let decoded = LaunchFeedback::decode(bytes.as_ref()).expect("decode");
        assert_eq!(decoded, fb);
        let LaunchFeedback::Output(output) = decoded else {
            panic!("a line of output decodes as one: {decoded:?}");
        };
        assert!(output.is_stderr());
        assert!(!output.is_stdout());
        assert_eq!(output.line, "something failed");
    }

    #[test]
    fn launch_feedback_decode_rejects_malformed() {
        assert!(LaunchFeedback::decode(b"not capnp").is_err());
    }

    /// The node logs of a goal ride its feedback, with the phase that
    /// failed, the core node that holds each file and the instance of each
    /// start.
    #[test]
    fn launch_feedback_node_logs_roundtrip() {
        let logs = NodeLogs {
            node_add_logs: vec![NodeAddLogEntry {
                node_label: "camera:v1".to_string(),
                log_path: PathBuf::from("/var/log/add/camera.log"),
                failed: false,
                core_node: "cn-coordinator".to_string(),
            }],
            node_build_logs: vec![NodeBuildLogEntry {
                node_label: "planner:v2".to_string(),
                log_path: PathBuf::from("/var/log/build/planner.log"),
                failed: true,
                core_node: "cn-gpu-box".to_string(),
            }],
            node_run_logs: vec![NodeRunLogEntry {
                instance_id: "inst-001".to_string(),
                node_label: "driver:v3".to_string(),
                log_path: PathBuf::from("/var/log/run/driver.log"),
                failed: false,
                core_node: "cn-robot-arm".to_string(),
            }],
        };
        let fb = LaunchFeedback::NodeLogs(logs.clone());
        let bytes = fb.encode().expect("encode").into_inner();
        assert_eq!(LaunchFeedback::decode(bytes.as_ref()).expect("decode"), fb);
        assert!(!logs.is_empty());
        assert_eq!(
            logs.failed().collect::<Vec<_>>(),
            [std::path::Path::new("/var/log/build/planner.log")]
        );

        let empty = LaunchFeedback::NodeLogs(NodeLogs::default());
        let bytes = empty.encode().expect("encode").into_inner();
        assert_eq!(
            LaunchFeedback::decode(bytes.as_ref()).expect("decode"),
            empty
        );
    }

    #[test]
    fn launch_goal_response_roundtrips() {
        let response = LaunchGoalResponse::new("/var/log/launch.log");
        let bytes = response.encode().expect("encode");
        assert_eq!(
            LaunchGoalResponse::decode(bytes.as_ref()).expect("decode"),
            response
        );
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
        let result = LaunchResult::new(vec![
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

        let empty = LaunchResult::default();
        let decoded =
            LaunchResult::decode(empty.encode().expect("encode").as_ref()).expect("decode");
        assert!(decoded.instance_endpoints.is_empty());
    }
}
