//! The log file of one action (a stack change, a node add, build or run) and
//! the lines written to it.
//!
//! Every line goes through [`ActionLog`], which writes it to the file and
//! hands it to the log exporter with the identity of the log. [`Announcer`]
//! pairs a log with the feedback channel of its action, for the steps the
//! daemon reports to both.

use crate::build_io::{FeedbackLine, FeedbackStream};
use chrono::{DateTime, Local};
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::Severity;
use log_export::{Iostream, LogExporter, LogIdentity, LogKind, StackAction};
use parking_lot::Mutex;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// The format of the local time that starts every line of a log file.
const LINE_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f";
/// The format of the local time in the name of a log file.
const FILE_TIMESTAMP_FORMAT: &str = "%Y%m%d_%H%M%S_%3f";
/// The marker of an error the daemon writes.
const ERROR_MARKER: &str = "error";
/// What separates an error from the last lines its command printed on stderr,
/// which the daemon quotes after it.
pub const STDERR_TAIL_HEADER: &str = "\n\n--- stderr (last lines) ---\n";
/// How far the time of a line may run ahead of the clock. Past it, a clock
/// that stepped back is followed.
const MAX_CLOCK_LEAD: Duration = Duration::from_secs(1);

/// One line of a log file: `[time] [marker] text`, or `[time] text` for a
/// line with no marker. `time` is shown as local time.
pub fn log_line(time: SystemTime, marker: Option<&str>, text: &str) -> String {
    let timestamp = DateTime::<Local>::from(time).format(LINE_TIMESTAMP_FORMAT);
    match marker {
        Some(marker) => format!("[{timestamp}] [{marker}] {text}"),
        None => format!("[{timestamp}] {text}"),
    }
}

/// The log file of one action. Cloning it shares the file.
///
/// Writes are best-effort: a line the file refuses is still exported, and the
/// action goes on.
#[derive(Clone)]
pub struct ActionLog {
    inner: Arc<Inner>,
}

struct Inner {
    exporter: LogExporter,
    /// The timestamp in the name of the file, when its name holds one.
    file_timestamp: Option<String>,
    state: Mutex<State>,
}

struct State {
    file: File,
    identity: Arc<LogIdentity>,
    last_time: SystemTime,
}

/// What the exporter receives of a line.
enum Exported {
    /// Nothing: the line is in the file only.
    No,
    /// A line the daemon wrote itself.
    Daemon(Severity),
    /// A line captured from a node or a build.
    Captured(Iostream),
}

impl ActionLog {
    /// Creates the log file `filename` in `dir`, and `dir` when it is absent.
    /// `filename` is one path component, so a name that holds client input
    /// stays inside `dir`.
    pub fn create(
        dir: &Path,
        filename: &str,
        kind: LogKind,
        exporter: LogExporter,
    ) -> Result<Self, String> {
        Self::create_as(dir, filename, None, exporter, |path| {
            LogIdentity::new(kind, path)
        })
    }

    /// The launch log of the stack `action`, `{action}_{timestamp}.log`.
    pub fn for_stack_action(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        action: StackAction,
        launch_id: Option<&str>,
    ) -> Result<Self, String> {
        let timestamp = file_timestamp();
        let filename = format!("{}_{timestamp}.log", action.name());
        Self::create_as(
            &peppy_dirs.logs_dir_launch(),
            &filename,
            Some(timestamp),
            exporter,
            |path| LogIdentity::new(LogKind::Launch { action }, path).with_launch(launch_id),
        )
    }

    /// The add log of a source, `{label}_{timestamp}.log`. `node` is the
    /// `(name, tag)` of the node the source names, when it names one;
    /// [`Self::rename_for_node`] names the node once its config is read.
    pub fn for_add(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        label: &str,
        node: Option<(&str, &str)>,
        launch_id: Option<&str>,
    ) -> Result<Self, String> {
        Self::for_add_at(
            peppy_dirs,
            exporter,
            label,
            node,
            launch_id,
            file_timestamp(),
        )
    }

    /// [`Self::for_add`] with the timestamp of the file name chosen by a test.
    fn for_add_at(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        label: &str,
        node: Option<(&str, &str)>,
        launch_id: Option<&str>,
        timestamp: String,
    ) -> Result<Self, String> {
        let filename = format!("{label}_{timestamp}.log");
        Self::create_as(
            &peppy_dirs.logs_dir_add(),
            &filename,
            Some(timestamp),
            exporter,
            |path| {
                let identity = LogIdentity::new(LogKind::Add, path).with_launch(launch_id);
                match node {
                    Some((name, tag)) => identity.with_node(name, tag),
                    None => identity,
                }
            },
        )
    }

    /// The build log of the node `name:tag`, `{name}_{tag}_{timestamp}.log`.
    pub fn for_build(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        name: &str,
        tag: &str,
        launch_id: Option<&str>,
    ) -> Result<Self, String> {
        let timestamp = file_timestamp();
        let filename = format!("{name}_{tag}_{timestamp}.log");
        Self::create_as(
            &peppy_dirs.logs_dir_build(),
            &filename,
            Some(timestamp),
            exporter,
            |path| {
                LogIdentity::new(LogKind::Build, path)
                    .with_node(name, tag)
                    .with_launch(launch_id)
            },
        )
    }

    /// The run log of the instance `instance_id` of the node `name:tag`,
    /// `{instance_id}.log`.
    pub fn for_run(
        peppy_dirs: &PeppyDirs,
        exporter: LogExporter,
        name: &str,
        tag: &str,
        instance_id: &str,
        launch_id: Option<&str>,
    ) -> Result<Self, String> {
        Self::create_as(
            &peppy_dirs.logs_dir_run(),
            &format!("{instance_id}.log"),
            None,
            exporter,
            |path| {
                LogIdentity::new(LogKind::Run, path)
                    .with_node(name, tag)
                    .with_instance(instance_id)
                    .with_launch(launch_id)
            },
        )
    }

    /// Creates the log file. A file whose name holds a timestamp is created
    /// beside a file of the same name, under the next free name; a file
    /// named after its instance alone replaces the one of the instance's
    /// last run.
    fn create_as(
        dir: &Path,
        filename: &str,
        file_timestamp: Option<String>,
        exporter: LogExporter,
        identify: impl FnOnce(PathBuf) -> LogIdentity,
    ) -> Result<Self, String> {
        validate_log_filename(filename)?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Failed to create logs directory: {e}"))?;
        let (file, path) = match file_timestamp {
            Some(_) => create_free(dir, filename),
            None => {
                let path = dir.join(filename);
                File::create(&path).map(|file| (file, path))
            }
        }
        .map_err(|e| format!("Failed to create log file: {e}"))?;
        let state = State {
            file,
            identity: Arc::new(identify(path)),
            last_time: SystemTime::UNIX_EPOCH,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                exporter,
                file_timestamp,
                state: Mutex::new(state),
            }),
        })
    }

    /// The path of the log file.
    pub fn path(&self) -> PathBuf {
        self.inner.state.lock().identity.file_path.clone()
    }

    /// Names the log after the node it turned out to belong to: every later
    /// line belongs to the node, and a file whose name holds a timestamp
    /// becomes `{name}_{tag}_{timestamp}.log` in the same directory. A file
    /// that cannot be renamed keeps its name.
    pub fn rename_for_node(&self, name: &str, tag: &str) {
        let mut state = self.inner.state.lock();
        let names_the_node = state
            .identity
            .node
            .as_ref()
            .is_some_and(|node| node.name == name && node.tag == tag);
        if names_the_node {
            return;
        }
        let current = state.identity.file_path.clone();
        let file_path = self
            .inner
            .file_timestamp
            .as_ref()
            .map(|timestamp| format!("{name}_{tag}_{timestamp}.log"))
            .filter(|filename| validate_log_filename(filename).is_ok())
            .and_then(|filename| move_to_free_name(&current, &filename).ok())
            .unwrap_or(current);
        let identity = LogIdentity {
            file_path,
            ..(*state.identity).clone()
        };
        state.identity = Arc::new(identity.with_node(name, tag));
    }

    /// Writes a line captured from a node or a build.
    pub fn output(&self, stream: Iostream, line: &str) {
        self.write(Some(stream.name()), line, Exported::Captured(stream));
    }

    /// Writes a fragment of a line that a progress bar repaints. It stays in
    /// the file.
    pub fn repaint(&self, stream: Iostream, fragment: &str) {
        self.write(Some(stream.name()), fragment, Exported::No);
    }

    /// Writes a step the daemon reports.
    pub fn note(&self, line: &str) {
        self.narrate(FeedbackStream::Stdout, Severity::Info, line);
    }

    /// Writes a warning of the daemon.
    pub fn warning(&self, line: &str) {
        self.narrate(FeedbackStream::Warning, Severity::Warn, line);
    }

    /// Writes an error of the daemon. The record of an error that quotes the
    /// stderr of its command after [`STDERR_TAIL_HEADER`] holds the error
    /// alone: each quoted line is a record of the log that captured it.
    pub fn error(&self, message: &str) {
        self.write(
            Some(ERROR_MARKER),
            message,
            Exported::Daemon(Severity::Error),
        );
    }

    /// Writes a line of the daemon's narration, which goes to the terminal on
    /// `stream` and is exported at `severity`.
    pub fn narrate(&self, stream: FeedbackStream, severity: Severity, line: &str) {
        self.write(Some(stream.as_str()), line, Exported::Daemon(severity));
    }

    /// Writes the command the daemon is about to run, as
    /// `Executing {label}: {cmd} (working_dir: {dir}[, key: value...])`.
    pub fn command(&self, label: &str, cmd: &str, working_dir: &Path, extras: &[(&str, &str)]) {
        let extras: String = extras
            .iter()
            .map(|(key, value)| format!(", {key}: {value}"))
            .collect();
        let line = format!(
            "Executing {label}: {cmd} (working_dir: {}{extras})",
            working_dir.display()
        );
        self.write(None, &line, Exported::Daemon(Severity::Info));
    }

    /// Writes a line that the log of another action holds and exports.
    pub fn relay(&self, stream: FeedbackStream, line: &str) {
        self.write(Some(stream.as_str()), line, Exported::No);
    }

    /// Writes one line and exports it, under one lock, so the records of a
    /// log leave in the order of its lines.
    fn write(&self, marker: Option<&str>, text: &str, exported: Exported) {
        let mut state = self.inner.state.lock();
        let time = time_after(state.last_time, SystemTime::now());
        state.last_time = time;
        let _ = writeln!(state.file, "{}", log_line(time, marker, text));
        match exported {
            Exported::No => {}
            Exported::Daemon(severity) => {
                let own_text = text
                    .split_once(STDERR_TAIL_HEADER)
                    .map_or(text, |(error, _quoted)| error);
                self.inner
                    .exporter
                    .export_daemon_line(&state.identity, time, severity, own_text);
            }
            Exported::Captured(stream) => {
                self.inner
                    .exporter
                    .export_captured_line(&state.identity, time, stream, text);
            }
        }
    }
}

/// Creates `filename` in `dir`, or the first of `filename`'s free names: a
/// log created in the same millisecond as another of the same name sits
/// beside it.
fn create_free(dir: &Path, filename: &str) -> std::io::Result<(File, PathBuf)> {
    let mut candidates = free_names(dir, filename);
    loop {
        let path = candidates.next().expect("the names never run out");
        match File::create_new(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Moves the file at `current` to `filename` in its directory, or to the
/// first of `filename`'s free names: a link to the new name is made first,
/// which fails when the name is taken, so no file is replaced.
fn move_to_free_name(current: &Path, filename: &str) -> std::io::Result<PathBuf> {
    let dir = current
        .parent()
        .ok_or_else(|| std::io::Error::other("a log file has a directory"))?;
    let mut candidates = free_names(dir, filename);
    loop {
        let path = candidates.next().expect("the names never run out");
        match std::fs::hard_link(current, &path) {
            Ok(()) => {
                std::fs::remove_file(current)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// `filename` in `dir`, then `{stem}-2.log`, `{stem}-3.log` and so on.
fn free_names(dir: &Path, filename: &str) -> impl Iterator<Item = PathBuf> {
    let stem = filename.strip_suffix(".log").unwrap_or(filename).to_owned();
    let dir = dir.to_path_buf();
    std::iter::once(dir.join(filename))
        .chain((2u32..).map(move |n| dir.join(format!("{stem}-{n}.log"))))
}

/// The local time for the name of a log file created now.
fn file_timestamp() -> String {
    Local::now().format(FILE_TIMESTAMP_FORMAT).to_string()
}

/// The time of the line written at `now` after a line written at `last`:
/// `now`, or one nanosecond after `last` when the clock has not moved past
/// it, so the times of a log's lines strictly increase. A clock more than
/// [`MAX_CLOCK_LEAD`] behind `last` stepped back, and the line takes `now`.
pub fn time_after(last: SystemTime, now: SystemTime) -> SystemTime {
    match last.duration_since(now) {
        Ok(lead) if lead <= MAX_CLOCK_LEAD => last + Duration::from_nanos(1),
        _ => now,
    }
}

fn validate_log_filename(name: &str) -> Result<(), String> {
    let is_one_component = Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name);
    let is_reserved = name.is_empty() || name == "." || name == "..";
    let has_separator = name.contains('/') || name.contains('\\') || name.contains('\0');
    if is_reserved || has_separator || !is_one_component {
        return Err(format!("invalid log filename: {name:?}"));
    }
    Ok(())
}

/// A log and the feedback channel of its action: where the daemon reports the
/// steps of an add, a build or a run.
#[derive(Clone)]
pub struct Announcer {
    log: ActionLog,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
}

impl Announcer {
    pub fn new(log: ActionLog, feedback_tx: mpsc::UnboundedSender<FeedbackLine>) -> Self {
        Self { log, feedback_tx }
    }

    pub fn log(&self) -> &ActionLog {
        &self.log
    }

    pub fn feedback_tx(&self) -> &mpsc::UnboundedSender<FeedbackLine> {
        &self.feedback_tx
    }

    /// Reports a step: the line lands in the log and on the feedback channel
    /// as stdout.
    pub fn line(&self, line: impl Into<String>) {
        let line = line.into();
        self.log.note(&line);
        self.send(FeedbackLine::logged(FeedbackStream::Stdout, line));
    }

    /// Reports a warning: the line lands in the log and on the feedback
    /// channel as a warning, which the launch output keeps in view after the
    /// lines of the step scroll past.
    pub fn warning(&self, line: impl Into<String>) {
        let line = line.into();
        self.log.warning(&line);
        self.send(FeedbackLine::logged(FeedbackStream::Warning, line));
    }

    /// Reports a progress sample: a line that repeats while a transfer or a
    /// build goes on. It reaches the feedback channel only.
    pub fn progress(&self, line: impl Into<String>) {
        self.send(FeedbackLine::progress(line.into()));
    }

    /// A closed channel means the consumer of the feedback is gone.
    fn send(&self, line: FeedbackLine) {
        let _ = self.feedback_tx.send(line);
    }
}

/// Readers of the log file format, for the tests of this crate and of
/// downstream crates.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    /// A log that exports nothing, for a test that needs a place to write.
    /// Its file is unlinked at once, so it leaves nothing behind.
    #[cfg(test)]
    pub(crate) fn scratch_log() -> super::ActionLog {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        super::ActionLog::create(
            dir.path(),
            "scratch.log",
            log_export::LogKind::Build,
            log_export::LogExporter::disabled(),
        )
        .expect("create the scratch log")
    }

    /// The text of `line` after its `[time] ` prefix, or `None` for a line
    /// that continues an entry.
    fn after_time_prefix(line: &str) -> Option<&str> {
        let width = super::log_line(std::time::UNIX_EPOCH, None, "").len();
        let prefix = line.get(..width)?;
        let shaped = prefix.starts_with('[')
            && prefix.ends_with("] ")
            && prefix[1..5].bytes().all(|byte| byte.is_ascii_digit());
        shaped.then(|| &line[width..])
    }

    /// The lines of a log file's `content` after their `[time] ` prefix, each
    /// with its `[marker]` when it has one.
    pub fn lines_without_time(content: &str) -> Vec<String> {
        content
            .lines()
            .map(|line| {
                after_time_prefix(line)
                    .expect("a line starts with its time")
                    .to_owned()
            })
            .collect()
    }

    /// The entries of a log file's `content`: the text of each line after its
    /// `[time]` prefix and its `[marker]`, with the lines that continue an entry
    /// joined to it.
    pub fn log_entries(content: &str) -> Vec<String> {
        let markers = [
            super::FeedbackStream::Stdout.as_str(),
            super::FeedbackStream::Stderr.as_str(),
            super::FeedbackStream::Warning.as_str(),
            super::ERROR_MARKER,
        ]
        .map(|marker| format!("[{marker}] "));
        content.lines().fold(Vec::new(), |mut entries, line| {
            match (after_time_prefix(line), entries.last_mut()) {
                (None, Some(entry)) => {
                    entry.push('\n');
                    entry.push_str(line);
                }
                (None, None) => entries.push(line.to_owned()),
                (Some(text), _) => {
                    let text = markers
                        .iter()
                        .find_map(|marker| text.strip_prefix(marker.as_str()))
                        .unwrap_or(text);
                    entries.push(text.to_owned());
                }
            }
            entries
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_io::FeedbackOwner;
    use log_export::test_support::drain_records as records;
    use log_export::{LineOrigin, LogRecordReceiver};

    fn log_in(dir: &Path, kind: LogKind) -> (ActionLog, LogRecordReceiver) {
        let (exporter, receiver) = LogExporter::channel(None);
        let log = ActionLog::create(dir, "action.log", kind, exporter).expect("the log is created");
        (log, receiver)
    }

    fn lines_without_time(log: &ActionLog) -> Vec<String> {
        let content = std::fs::read_to_string(log.path()).expect("the log file is readable");
        test_support::lines_without_time(&content)
    }

    #[test]
    fn each_kind_of_line_has_its_marker_and_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let (log, mut receiver) = log_in(dir.path(), LogKind::Run);
        log.output(Iostream::Stdout, "INFO the node is ready");
        log.output(Iostream::Stderr, "a plain line");
        log.note("paired: a_inst with b_inst");
        log.warning("bind source created");
        log.error("the node did not start");
        log.narrate(
            FeedbackStream::Stderr,
            Severity::Warn,
            "a mount was created",
        );
        log.command(
            "run_cmd",
            "./node",
            Path::new("/work"),
            &[("bind_mounts", "[]")],
        );

        assert_eq!(
            lines_without_time(&log),
            [
                "[stdout] INFO the node is ready",
                "[stderr] a plain line",
                "[stdout] paired: a_inst with b_inst",
                "[warning] bind source created",
                "[error] the node did not start",
                "[stderr] a mount was created",
                "Executing run_cmd: ./node (working_dir: /work, bind_mounts: [])",
            ]
        );
        let exported: Vec<(LineOrigin, Option<Severity>, String)> = records(&mut receiver)
            .into_iter()
            .map(|record| (record.origin, record.severity, record.body))
            .collect();
        assert_eq!(
            exported,
            [
                (
                    LineOrigin::Captured(Iostream::Stdout),
                    Some(Severity::Info),
                    "INFO the node is ready".to_owned()
                ),
                (
                    LineOrigin::Captured(Iostream::Stderr),
                    None,
                    "a plain line".to_owned()
                ),
                (
                    LineOrigin::Daemon,
                    Some(Severity::Info),
                    "paired: a_inst with b_inst".to_owned()
                ),
                (
                    LineOrigin::Daemon,
                    Some(Severity::Warn),
                    "bind source created".to_owned()
                ),
                (
                    LineOrigin::Daemon,
                    Some(Severity::Error),
                    "the node did not start".to_owned()
                ),
                (
                    LineOrigin::Daemon,
                    Some(Severity::Warn),
                    "a mount was created".to_owned()
                ),
                (
                    LineOrigin::Daemon,
                    Some(Severity::Info),
                    "Executing run_cmd: ./node (working_dir: /work, bind_mounts: [])".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_relayed_line_and_a_repaint_stay_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let launch = LogKind::Launch {
            action: StackAction::Launch,
        };
        let (log, mut receiver) = log_in(dir.path(), launch);
        log.relay(FeedbackStream::Stdout, "output of a node");
        log.repaint(Iostream::Stderr, "Downloading 40%");

        assert_eq!(
            lines_without_time(&log),
            ["[stdout] output of a node", "[stderr] Downloading 40%"]
        );
        assert_eq!(records(&mut receiver), []);
    }

    #[test]
    fn a_record_carries_the_time_of_its_line_and_times_strictly_increase() {
        let dir = tempfile::tempdir().unwrap();
        let (log, mut receiver) = log_in(dir.path(), LogKind::Build);
        for line in ["one", "two", "three"] {
            log.output(Iostream::Stdout, line);
        }

        let records = records(&mut receiver);
        let file = std::fs::read_to_string(log.path()).unwrap();
        let written: Vec<&str> = file.lines().collect();
        let expected: Vec<String> = records
            .iter()
            .map(|record| log_line(record.time, Some("stdout"), &record.body))
            .collect();
        assert_eq!(written, expected);
        assert!(records.windows(2).all(|pair| pair[0].time < pair[1].time));
    }

    #[test]
    fn a_run_log_is_named_after_its_instance_and_names_what_it_belongs_to() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log = ActionLog::for_run(
            &dirs,
            exporter,
            "openarm_arm",
            "v1",
            "arm_inst",
            Some("launch-7"),
        )
        .unwrap();
        log.note("started");

        let path = dirs.logs_dir_run().join("arm_inst.log");
        assert_eq!(log.path(), path);
        let expected = LogIdentity::new(LogKind::Run, path)
            .with_node("openarm_arm", "v1")
            .with_instance("arm_inst")
            .with_launch(Some("launch-7"));
        assert_eq!(*records(&mut receiver)[0].identity, expected);
    }

    /// The name of the file at `path`, with the digits of its timestamp
    /// replaced by `#`.
    fn file_name_pattern(path: &Path) -> String {
        path.file_name()
            .and_then(|name| name.to_str())
            .expect("a log file has a name")
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect()
    }

    #[test]
    fn a_build_log_and_a_launch_log_are_named_after_what_they_log_and_when() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);

        let build =
            ActionLog::for_build(&dirs, exporter.clone(), "camera", "v", Some("launch-7")).unwrap();
        build.note("building");
        let launch =
            ActionLog::for_stack_action(&dirs, exporter, StackAction::Join, Some("launch-7"))
                .unwrap();
        launch.note("joining");

        assert_eq!(build.path().parent(), Some(dirs.logs_dir_build().as_path()));
        assert_eq!(
            file_name_pattern(&build.path()),
            "camera_v_########_######_###.log"
        );
        assert_eq!(
            launch.path().parent(),
            Some(dirs.logs_dir_launch().as_path())
        );
        assert_eq!(
            file_name_pattern(&launch.path()),
            "join_########_######_###.log"
        );
        let identities: Vec<LogIdentity> = records(&mut receiver)
            .into_iter()
            .map(|record| (*record.identity).clone())
            .collect();
        assert_eq!(
            identities,
            [
                LogIdentity::new(LogKind::Build, build.path())
                    .with_node("camera", "v")
                    .with_launch(Some("launch-7")),
                LogIdentity::new(
                    LogKind::Launch {
                        action: StackAction::Join
                    },
                    launch.path()
                )
                .with_launch(Some("launch-7")),
            ]
        );
    }

    #[test]
    fn an_add_log_takes_the_name_of_its_node_once_the_node_is_known() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log = ActionLog::for_add(&dirs, exporter, "source", None, None).unwrap();
        let created = log.path();
        assert_eq!(
            file_name_pattern(&created),
            "source_########_######_###.log"
        );
        log.note("Cloning repository");
        log.rename_for_node("camera", "v");
        log.note("Resolved dependency");
        // The same values again change nothing.
        log.rename_for_node("camera", "v");

        let renamed = log.path();
        assert_eq!(
            file_name_pattern(&renamed),
            "camera_v_########_######_###.log"
        );
        assert!(!created.exists());
        assert_eq!(
            lines_without_time(&log),
            [
                "[stdout] Cloning repository",
                "[stdout] Resolved dependency"
            ]
        );
        let records = records(&mut receiver);
        assert_eq!(
            *records[0].identity,
            LogIdentity::new(LogKind::Add, created)
        );
        assert_eq!(
            *records[1].identity,
            LogIdentity::new(LogKind::Add, renamed).with_node("camera", "v")
        );
    }

    #[test]
    fn two_logs_created_in_the_same_millisecond_sit_side_by_side() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let stamp = || "20260101_000000_000".to_owned();
        let exporter = LogExporter::disabled();
        let batch =
            ActionLog::for_add_at(&dirs, exporter.clone(), "source", None, None, stamp()).unwrap();
        let sub_add =
            ActionLog::for_add_at(&dirs, exporter.clone(), "source", None, None, stamp()).unwrap();
        batch.note("Adding camera:v");
        sub_add.note("Cloning repository");
        // Both turn out to belong to the same node.
        batch.rename_for_node("camera", "v");
        sub_add.rename_for_node("camera", "v");
        sub_add.rename_for_node("camera", "v");
        batch.note("Batch add complete");

        let add_dir = dirs.logs_dir_add();
        assert_eq!(
            batch.path(),
            add_dir.join("camera_v_20260101_000000_000.log")
        );
        assert_eq!(
            sub_add.path(),
            add_dir.join("camera_v_20260101_000000_000-2.log")
        );
        assert_eq!(
            lines_without_time(&batch),
            ["[stdout] Adding camera:v", "[stdout] Batch add complete"]
        );
        assert_eq!(
            lines_without_time(&sub_add),
            ["[stdout] Cloning repository"]
        );
        let mut names: Vec<String> = std::fs::read_dir(&add_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "camera_v_20260101_000000_000-2.log",
                "camera_v_20260101_000000_000.log",
            ]
        );
    }

    #[test]
    fn a_rename_to_a_name_that_is_no_file_name_keeps_the_file_and_names_the_node() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log = ActionLog::for_add(&dirs, exporter, "source", None, None).unwrap();
        let created = log.path();

        log.rename_for_node("camera", "v1/../../etc");
        log.note("Resolved dependency");

        assert_eq!(log.path(), created);
        assert!(created.exists());
        let record = &records(&mut receiver)[0];
        assert_eq!(record.identity.file_path, created);
        assert_eq!(
            record.identity.node.as_ref().map(|node| node.tag.as_str()),
            Some("v1/../../etc")
        );
    }

    #[test]
    fn an_add_log_of_a_source_that_names_its_node_belongs_to_the_node_from_its_first_line() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, mut receiver) = LogExporter::channel(None);
        let log =
            ActionLog::for_add(&dirs, exporter, "camera_v", Some(("camera", "v")), None).unwrap();
        log.note("Resolving camera:v from repo cache");

        let expected = LogIdentity::new(LogKind::Add, log.path()).with_node("camera", "v");
        assert_eq!(*records(&mut receiver)[0].identity, expected);
    }

    #[test]
    fn the_record_of_an_error_holds_the_error_without_the_stderr_it_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let (log, mut receiver) = log_in(dir.path(), LogKind::Build);
        let error =
            format!("apptainer build failed with status 1{STDERR_TAIL_HEADER}FATAL: no space");
        log.error(&error);
        log.narrate(
            FeedbackStream::Stderr,
            Severity::Error,
            &format!("Build failed: {error}"),
        );

        let file = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(file.matches("FATAL: no space").count(), 2);
        let bodies: Vec<String> = records(&mut receiver)
            .into_iter()
            .map(|record| record.body)
            .collect();
        assert_eq!(
            bodies,
            [
                "apptainer build failed with status 1",
                "Build failed: apptainer build failed with status 1",
            ]
        );
    }

    #[test]
    fn the_time_of_a_line_follows_the_clock_and_never_repeats() {
        let at = |millis: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
        let nanosecond = Duration::from_nanos(1);
        // The clock moved on.
        assert_eq!(time_after(at(1_000), at(1_001)), at(1_001));
        // The clock stands still, or is a little behind the last line.
        assert_eq!(time_after(at(1_000), at(1_000)), at(1_000) + nanosecond);
        assert_eq!(time_after(at(1_000), at(400)), at(1_000) + nanosecond);
        // The clock stepped back by more than a second.
        assert_eq!(time_after(at(5_000), at(1_000)), at(1_000));
    }

    #[test]
    fn the_entries_of_a_log_file_join_the_lines_that_continue_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (log, _receiver) = log_in(dir.path(), LogKind::Run);
        log.command("run_cmd", "./node", Path::new("/work"), &[]);
        log.output(Iostream::Stderr, "[node] ready");
        log.error("the node did not start\n\n--- stderr (last lines) ---\n[node] ready");

        let content = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(
            test_support::log_entries(&content),
            [
                "Executing run_cmd: ./node (working_dir: /work)",
                "[node] ready",
                "the node did not start\n\n--- stderr (last lines) ---\n[node] ready",
            ]
        );
    }

    #[test]
    fn a_log_filename_is_one_path_component() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "foo.log",
            "camera_0.1.0_20260101_000000_000.log",
            "a-b_c.log",
        ] {
            let log = ActionLog::create(dir.path(), name, LogKind::Add, LogExporter::disabled());
            assert!(log.is_ok(), "should accept `{name}`");
        }
        for name in [
            "",
            ".",
            "..",
            "../evil.log",
            "a/b.log",
            "a\\b.log",
            "a\0b.log",
        ] {
            let log = ActionLog::create(dir.path(), name, LogKind::Add, LogExporter::disabled());
            assert!(log.is_err(), "should reject `{name}`");
        }
    }

    #[test]
    fn an_announcer_reports_to_the_log_and_the_feedback_channel() {
        let dir = tempfile::tempdir().unwrap();
        let (log, mut receiver) = log_in(dir.path(), LogKind::Build);
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();
        let announcer = Announcer::new(log.clone(), feedback_tx);

        announcer.line("Reusing the cached build");
        announcer.warning("the mirror did not answer");
        announcer.progress("Build progress: 12 MB written");

        assert_eq!(
            lines_without_time(&log),
            [
                "[stdout] Reusing the cached build",
                "[warning] the mirror did not answer"
            ]
        );
        assert_eq!(records(&mut receiver).len(), 2);
        let feedback: Vec<(FeedbackStream, String, FeedbackOwner)> =
            std::iter::from_fn(|| feedback_rx.try_recv().ok())
                .map(|line| (line.stream, line.line.clone(), line.owner()))
                .collect();
        assert_eq!(
            feedback,
            [
                (
                    FeedbackStream::Stdout,
                    "Reusing the cached build".to_owned(),
                    FeedbackOwner::ActionLog
                ),
                (
                    FeedbackStream::Warning,
                    "the mirror did not answer".to_owned(),
                    FeedbackOwner::ActionLog
                ),
                (
                    FeedbackStream::Stdout,
                    "Build progress: 12 MB written".to_owned(),
                    FeedbackOwner::Progress
                ),
            ]
        );

        // A step still lands in the log once the consumer of the feedback is
        // gone.
        drop(feedback_rx);
        announcer.line("after the consumer left");
        assert_eq!(lines_without_time(&log).len(), 3);
    }
}
