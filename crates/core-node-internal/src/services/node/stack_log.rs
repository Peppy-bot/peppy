//! The stack log: what became of each instance of the stack, and what the
//! log export discarded.

use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::Severity;
use log_export::{LogExporter, LogIdentity, LogKind};
use node_stack::action_log::{log_line, time_after};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

/// The stack log file and the exporter of its lines, with the identity the
/// lines of one writer carry.
#[derive(Clone)]
struct StackLogWriter {
    identity: Arc<LogIdentity>,
    exporter: LogExporter,
}

impl StackLogWriter {
    /// Appends `message` to the stack log as one line, and exports it. The
    /// write is best-effort: what the line describes has already happened,
    /// and it is exported either way.
    fn write(&self, severity: Severity, message: &str) {
        let time = next_stack_time();
        append(&self.identity.file_path, &log_line(time, None, message));
        self.exporter
            .export_daemon_line(&self.identity, time, severity, message);
    }
}

/// The stack log, as one instance writes to it: each event is appended to the
/// file and exported with the identity of the instance.
#[derive(Clone)]
pub(crate) struct StackLog {
    writer: StackLogWriter,
    /// The instance, as each line names it: `Instance '{id}' of node
    /// '{name}:{tag}'`.
    subject: String,
}

/// What happened to an instance.
pub(crate) enum StackLogEvent {
    /// The process exited on its own with success.
    Finished,
    /// The process exited on its own with a failure. `detail` is its exit
    /// status, or why the status is unknown.
    Failed { detail: String },
    /// `misses` health checks in a row failed. `reason` is the last failure.
    Unhealthy { misses: u32, reason: String },
    /// A health check passed after `misses` failed ones.
    Recovered { misses: u32 },
}

impl StackLogEvent {
    fn severity(&self) -> Severity {
        match self {
            StackLogEvent::Finished | StackLogEvent::Recovered { .. } => Severity::Info,
            StackLogEvent::Unhealthy { .. } => Severity::Warn,
            StackLogEvent::Failed { .. } => Severity::Error,
        }
    }

    /// What the event says about its instance, after the instance is named.
    fn outcome(&self) -> String {
        match self {
            StackLogEvent::Finished => "finished: process exited cleanly".to_owned(),
            StackLogEvent::Failed { detail } => format!("failed: process {detail}"),
            StackLogEvent::Unhealthy { misses, reason } => format!(
                "became unhealthy: {misses} consecutive health checks failed, last: {reason}"
            ),
            StackLogEvent::Recovered { misses } => {
                format!("recovered after {misses} missed health checks")
            }
        }
    }
}

impl StackLog {
    /// The stack log of the instance `instance_id` of `node_name:node_tag`,
    /// started by the launch `launch_id` when it is part of one.
    pub(crate) fn for_instance(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        instance_id: &str,
        node_name: &str,
        node_tag: &str,
        launch_id: Option<&str>,
    ) -> Self {
        let identity = LogIdentity::new(LogKind::Stack, peppy_dirs.stack_log_path())
            .with_node(node_name, node_tag)
            .with_instance(instance_id)
            .with_launch(launch_id);
        Self {
            writer: StackLogWriter {
                identity: Arc::new(identity),
                exporter,
            },
            subject: format!("Instance '{instance_id}' of node '{node_name}:{node_tag}'"),
        }
    }

    /// Records `event`.
    pub(crate) fn record(&self, event: &StackLogEvent) {
        self.writer.write(
            event.severity(),
            &format!("{} {}", self.subject, event.outcome()),
        );
    }
}

/// The stack log, as the log export writes to it: how many records the export
/// discarded. The line is exported like every other line of the stack log.
pub struct ExportDiscardLog {
    writer: StackLogWriter,
}

impl ExportDiscardLog {
    pub fn new(peppy_dirs: &PeppyDirs, exporter: LogExporter) -> Self {
        let identity = LogIdentity::new(LogKind::Stack, peppy_dirs.stack_log_path());
        Self {
            writer: StackLogWriter {
                identity: Arc::new(identity),
                exporter,
            },
        }
    }

    /// Records that the export discarded `count` records.
    pub fn record(&self, count: u64) {
        let records = if count == 1 { "record" } else { "records" };
        self.writer.write(
            Severity::Warn,
            &format!(
                "Log export discarded {count} {records} while the queue was full or the \
                 endpoint refused requests"
            ),
        );
    }
}

/// The time of the next stack log line: the writers share one clock, so the
/// times of the stack log's lines strictly increase.
fn next_stack_time() -> SystemTime {
    static LAST_TIME: LazyLock<Mutex<SystemTime>> =
        LazyLock::new(|| Mutex::new(SystemTime::UNIX_EPOCH));
    let mut last = LAST_TIME
        .lock()
        .expect("the stack log clock is not poisoned");
    *last = time_after(*last, SystemTime::now());
    *last
}

/// Appends `line` to the file at `path` in one write, so the lines of two
/// writers do not interleave.
fn append(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = file.write_all(format!("{line}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use log_export::LineOrigin;
    use log_export::test_support::drain_records;

    fn stack_log(dirs: &PeppyDirs, exporter: LogExporter) -> StackLog {
        StackLog::for_instance(
            dirs,
            exporter,
            "arm_inst",
            "openarm_arm",
            "v1",
            Some("launch-7"),
        )
    }

    fn lines_without_time(dirs: &PeppyDirs) -> Vec<String> {
        let content =
            std::fs::read_to_string(dirs.stack_log_path()).expect("the stack log is readable");
        node_stack::action_log::test_support::lines_without_time(&content)
    }

    #[test]
    fn each_event_is_appended_and_exported_at_its_severity() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log = stack_log(&dirs, exporter);

        let events = [
            (
                StackLogEvent::Finished,
                Severity::Info,
                "Instance 'arm_inst' of node 'openarm_arm:v1' finished: process exited cleanly",
            ),
            (
                StackLogEvent::Failed {
                    detail: "exit status exit status: 3".to_owned(),
                },
                Severity::Error,
                "Instance 'arm_inst' of node 'openarm_arm:v1' failed: process exit status exit \
                 status: 3",
            ),
            (
                StackLogEvent::Unhealthy {
                    misses: 3,
                    reason: "timed out".to_owned(),
                },
                Severity::Warn,
                "Instance 'arm_inst' of node 'openarm_arm:v1' became unhealthy: 3 consecutive \
                 health checks failed, last: timed out",
            ),
            (
                StackLogEvent::Recovered { misses: 4 },
                Severity::Info,
                "Instance 'arm_inst' of node 'openarm_arm:v1' recovered after 4 missed health \
                 checks",
            ),
        ];
        for (event, _, _) in &events {
            log.record(event);
        }

        let expected_lines: Vec<&str> = events.iter().map(|(_, _, line)| *line).collect();
        assert_eq!(lines_without_time(&dirs), expected_lines);

        let records = drain_records(&mut receiver);
        let exported: Vec<(Option<Severity>, &str)> = records
            .iter()
            .map(|record| (record.severity, record.body.as_str()))
            .collect();
        let expected_records: Vec<(Option<Severity>, &str)> = events
            .iter()
            .map(|(_, severity, line)| (Some(*severity), *line))
            .collect();
        assert_eq!(exported, expected_records);

        let expected_identity = LogIdentity::new(LogKind::Stack, dirs.stack_log_path())
            .with_node("openarm_arm", "v1")
            .with_instance("arm_inst")
            .with_launch(Some("launch-7"));
        for record in &records {
            assert_eq!(*record.identity, expected_identity);
            assert_eq!(record.origin, LineOrigin::Daemon);
        }
    }

    #[test]
    fn a_discard_report_is_a_warning_of_the_stack_log_that_belongs_to_no_node() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log = ExportDiscardLog::new(&dirs, exporter);

        log.record(1);
        log.record(4096);

        let expected = [
            "Log export discarded 1 record while the queue was full or the endpoint refused \
             requests",
            "Log export discarded 4096 records while the queue was full or the endpoint refused \
             requests",
        ];
        assert_eq!(lines_without_time(&dirs), expected);
        let records = drain_records(&mut receiver);
        let bodies: Vec<&str> = records.iter().map(|record| record.body.as_str()).collect();
        assert_eq!(bodies, expected);
        for record in &records {
            assert_eq!(record.severity, Some(Severity::Warn));
            assert_eq!(
                *record.identity,
                LogIdentity::new(LogKind::Stack, dirs.stack_log_path())
            );
        }
    }

    #[test]
    fn events_append_to_the_lines_the_file_already_holds() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        stack_log(&dirs, LogExporter::disabled()).record(&StackLogEvent::Finished);
        stack_log(&dirs, LogExporter::disabled()).record(&StackLogEvent::Recovered { misses: 1 });

        assert_eq!(lines_without_time(&dirs).len(), 2);
    }
}
