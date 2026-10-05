//! The record model: one exported record per line of a log file.

use daemon_config::peppy_config::Severity;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// The stack action a launch log is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackAction {
    Launch,
    Build,
    Join,
    Remove,
}

impl StackAction {
    /// The name of the action, as its command spells it.
    pub fn name(self) -> &'static str {
        match self {
            StackAction::Launch => "launch",
            StackAction::Build => "build",
            StackAction::Join => "join",
            StackAction::Remove => "remove",
        }
    }
}

/// Which log file a line was written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// A launch log, with the stack action that wrote it.
    Launch {
        action: StackAction,
    },
    Add,
    Build,
    Run,
    Stack,
}

impl LogKind {
    /// The name of the kind: the directory of its files under `logs/`, or
    /// `stack` for the stack log.
    pub(crate) fn name(self) -> &'static str {
        match self {
            LogKind::Launch { .. } => "launch",
            LogKind::Add => "add",
            LogKind::Build => "build",
            LogKind::Run => "run",
            LogKind::Stack => "stack",
        }
    }
}

/// The node a log belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRef {
    pub name: String,
    pub tag: String,
}

/// What every line of one log has in common: the file, its kind, and the
/// node, instance and launch the log belongs to when it belongs to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogIdentity {
    pub kind: LogKind,
    pub file_path: PathBuf,
    pub node: Option<NodeRef>,
    pub instance_id: Option<String>,
    pub launch_id: Option<String>,
}

impl LogIdentity {
    /// The identity of a log that belongs to no node, instance or launch.
    pub fn new(kind: LogKind, file_path: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            file_path: file_path.into(),
            node: None,
            instance_id: None,
            launch_id: None,
        }
    }

    pub fn with_node(self, name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            node: Some(NodeRef {
                name: name.into(),
                tag: tag.into(),
            }),
            ..self
        }
    }

    pub fn with_instance(self, instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: Some(instance_id.into()),
            ..self
        }
    }

    pub fn with_launch(self, launch_id: Option<&str>) -> Self {
        Self {
            launch_id: launch_id.map(str::to_owned),
            ..self
        }
    }
}

/// The output stream a captured line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Iostream {
    Stdout,
    Stderr,
}

impl Iostream {
    pub fn name(self) -> &'static str {
        match self {
            Iostream::Stdout => "stdout",
            Iostream::Stderr => "stderr",
        }
    }
}

/// Who wrote a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOrigin {
    /// The daemon wrote the line itself.
    Daemon,
    /// The daemon captured the line from a node or a build.
    Captured(Iostream),
}

/// One line of a log file, as exported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub identity: Arc<LogIdentity>,
    /// The daemon's clock when it wrote or read the line.
    pub time: SystemTime,
    pub origin: LineOrigin,
    pub severity: Option<Severity>,
    /// The level as the line prints it, or the name of the daemon's level.
    pub severity_text: Option<String>,
    /// The text of the line, without terminal escape sequences.
    pub body: String,
    /// Whether `body` is the start of a longer line.
    pub truncated: bool,
}
