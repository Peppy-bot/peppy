//! Real-binary end-to-end coverage of the goals a command line process
//! sends: each `peppy` process is a sender of its own, the daemon cancels
//! the goal of a process that is gone, and a Ctrl-C of a command cancels its
//! goal.
//!
//! Linux only: the managed zenohd is what lets a second process reach the
//! daemon.

#![cfg(target_os = "linux")]

use super::common::{ZenohDaemon, install_node_manifest, poll_for, publish_repo_index};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const CORE_NODE_NAME: &str = "cn-cli-goal-e2e";
const NODE_NAME: &str = "waiting_node";
const NODE_TAG: &str = "v1";
const INSTANCE_ID: &str = "waiting_inst";
/// How long a step of the daemon gets: a start, a cancel and its rollback.
const STEP_BOUND: Duration = Duration::from_secs(120);
/// How long the daemon gets to exit.
const DAEMON_EXIT: Duration = Duration::from_secs(30);

/// The repositories that every daemon reads by default. The test excludes
/// them, so no command reads the network.
const DEFAULT_REPOSITORIES: &str =
    include_str!("../../core-node-internal/assets/default_repositories.json5");

/// Boots a daemon that excludes the default repositories and reads the fs
/// repository `repository`.
fn boot(repository: &Path) -> ZenohDaemon {
    let repositories = format!(
        r#"[{{ id: 1, type: "fs", path: {:?} }}]"#,
        repository.display().to_string()
    );
    ZenohDaemon::boot(CORE_NODE_NAME, "", &[], move |dirs| {
        std::fs::write(
            dirs.conf_dir().join("excluded_repositories.json5"),
            DEFAULT_REPOSITORIES,
        )
        .expect("write the excluded repositories");
        std::fs::write(dirs.conf_dir().join("repositories.json5"), &repositories)
            .expect("write the repositories");
    })
}

/// Writes the node whose instance never signals ready, so its start waits,
/// and a launcher of one instance of it, which the repository caches
/// resolve. Returns the nodes directory, which holds both, and the
/// launcher.
fn write_the_waiting_launch(daemon: &ZenohDaemon) -> (tempfile::TempDir, PathBuf) {
    let manifest = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{NODE_NAME}", tag: "{NODE_TAG}" }},
            execution: {{
                language: "rust",
                build_cmd: ["sh", "-c", "true"],
                run_cmd: ["sleep", "300"]
            }}
        }}"#
    );
    let (nodes, node_dir) = daemon.install_node(NODE_NAME, &manifest);
    super::common::register_repo_caches(daemon.home.path(), &[(NODE_NAME, NODE_TAG, &node_dir)]);
    let launcher = nodes.path().join("peppy_launcher.json5");
    std::fs::write(
        &launcher,
        format!(
            r#"{{ peppy_schema: "launcher/v1", deployments: [ {{ source: {{ name: "{NODE_NAME}:{NODE_TAG}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}" }} ] }} ] }}"#
        ),
    )
    .expect("write the launcher");
    (nodes, launcher)
}

/// Whether `peppy stack list` on `daemon` lists the instance of the launch.
fn lists_the_instance(daemon: &ZenohDaemon) -> bool {
    daemon.peppy(&["stack", "list"]).contains(INSTANCE_ID)
}

/// Starts `peppy` with `args` against `daemon` as a process of its own.
fn spawn_peppy(daemon: &ZenohDaemon, args: &[&str]) -> Child {
    daemon
        .peppy_command(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start peppy")
}

/// What `child`, which ended, printed.
fn printed_by(child: Child) -> (std::process::ExitStatus, String) {
    let output = child.wait_with_output().expect("read the output of peppy");
    (
        output.status,
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// The lines that a command prints, on its stdout or on its stderr, which
/// the test reads as they come, and the ones it read so far.
struct PrintedLines {
    lines: mpsc::Receiver<String>,
    read: Vec<String>,
}

impl PrintedLines {
    /// Reads each line that `child` prints until the child closes its
    /// stdout and its stderr.
    fn of(child: &mut Child) -> Self {
        let (line_tx, lines) = mpsc::channel();
        let stdout: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped stdout"));
        let stderr: Box<dyn Read + Send> = Box::new(child.stderr.take().expect("piped stderr"));
        for stream in [stdout, stderr] {
            let line_tx = line_tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if line_tx.send(line).is_err() {
                        return;
                    }
                }
            });
        }
        Self {
            lines,
            read: Vec::new(),
        }
    }

    /// Reads lines until one holds `text`. Fails when the command prints no
    /// such line within `bound`, or ends before.
    fn wait_for(&mut self, text: &str, bound: Duration) {
        let deadline = std::time::Instant::now() + bound;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(line) = self.lines.recv_timeout(left) else {
                panic!(
                    "peppy printed no line with `{text}`; it printed:\n{}",
                    self.read.join("\n")
                );
            };
            let found = line.contains(text);
            self.read.push(line);
            if found {
                return;
            }
        }
    }

    /// Every line the command printed, once it closed its output.
    fn all(mut self) -> String {
        self.read.extend(self.lines.iter());
        self.read.join("\n")
    }
}

/// A launch log of `daemon` that holds `text`.
fn launch_log_holding(daemon: &ZenohDaemon, text: &str) -> Option<PathBuf> {
    std::fs::read_dir(daemon.dirs().logs_dir_launch())
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| std::fs::read_to_string(path).is_ok_and(|content| content.contains(text)))
}

/// A repository of one node whose index is a named pipe: a refresh that
/// reads it waits until the test writes the index. Returns the repository
/// and the writer of its index, which waits for the reader.
fn piped_repository(root: &Path) -> (PathBuf, PipedIndex) {
    let repository = root.join("piped_repository");
    let node_dir = install_node_manifest(
        &repository,
        "piped_node",
        "unused-hash",
        r#"{ peppy_schema: "node/v1", manifest: { name: "piped_node", tag: "v1" }, execution: { language: "rust", run_cmd: ["true"] } }"#,
    );
    assert!(node_dir.exists());
    publish_repo_index(&repository);
    let index_path = repository.join(daemon_config::consts::REPOSITORY_INDEX_FILE);
    let index = std::fs::read_to_string(&index_path).expect("read the index");
    (repository, PipedIndex { index_path, index })
}

/// The index of a piped repository, and where its pipe goes.
struct PipedIndex {
    index_path: PathBuf,
    index: String,
}

impl PipedIndex {
    /// Puts the pipe in place of the index file, and starts its writer. The
    /// writer says when a reader opened the pipe, and writes the index when
    /// the returned sender lets it.
    fn open(
        self,
    ) -> (
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        std::fs::remove_file(&self.index_path).expect("remove the index");
        let made = std::process::Command::new("mkfifo")
            .arg(&self.index_path)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo makes the pipe");
        let (opened_tx, opened_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel::<()>();
        let writer = std::thread::spawn(move || {
            let mut pipe = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.index_path)
                .expect("open the pipe");
            opened_tx.send(()).expect("the test waits for the reader");
            write_rx.recv().expect("the test lets the writer write");
            pipe.write_all(self.index.as_bytes())
                .expect("write the index");
        });
        (opened_rx, write_tx, writer)
    }
}

/// Each `peppy` process is a sender of its own. A process that is gone
/// gets its stack goal cancelled, "its sender is gone", and the launch
/// clears the stack as a failed launch does. The goal of another `peppy`
/// process, a refresh that waits on its read, goes on to its end.
#[test]
fn a_cli_process_that_is_gone_gets_its_stack_goal_cancelled_and_another_one_goes_on() {
    let scratch = tempfile::tempdir().expect("temp scratch dir");
    let (repository, piped_index) = piped_repository(scratch.path());
    let daemon = boot(&repository);
    let (_nodes, launcher) = write_the_waiting_launch(&daemon);

    // The launch starts the instance, which never signals ready.
    let mut launch = spawn_peppy(
        &daemon,
        &["stack", "launch", &launcher.display().to_string()],
    );
    poll_for(STEP_BOUND, "the launch to start its instance", || {
        lists_the_instance(&daemon).then_some(())
    });

    // Another process refreshes the repositories, and waits on the read of
    // the piped index.
    let (opened, write, writer) = piped_index.open();
    let refresh = spawn_peppy(&daemon, &["repo", "refresh"]);
    opened
        .recv_timeout(STEP_BOUND)
        .expect("the refresh reads the piped index");

    launch.kill().expect("kill the launch process");
    let _ = launch.wait();
    poll_for(
        STEP_BOUND,
        "the cancel of the launch to clear the stack",
        || (!lists_the_instance(&daemon)).then_some(()),
    );
    let log = poll_for(STEP_BOUND, "the launch log to say why it ended", || {
        launch_log_holding(&daemon, "cancelled: its sender is gone")
    });
    assert!(log.exists());

    write.send(()).expect("the writer waits");
    writer.join().expect("the writer does not panic");
    let (status, printed) = printed_by(refresh);
    assert!(
        status.success(),
        "the refresh of the process that stays ends: {printed}"
    );
    assert!(
        printed.contains("Repository refresh complete"),
        "the refresh completes: {printed}"
    );

    let (_home, _printed) = daemon.stop(DAEMON_EXIT);
}

/// A Ctrl-C of `peppy stack launch` sends the daemon a cancel. The command
/// says so, follows the launch to the end the daemon gives it, says that
/// the launch was cancelled by its caller, and exits with code 130. The
/// cancel clears the stack. The test sends SIGINT once the command follows
/// the goal, which it shows by a line of the goal's feedback, and once the
/// launch starts its instance, which never signals ready.
#[test]
fn a_ctrl_c_cancels_the_launch_and_the_command_exits_with_130() {
    let scratch = tempfile::tempdir().expect("temp scratch dir");
    // The test opens no pipe, so the index of this repository stays a file.
    let (repository, _index) = piped_repository(scratch.path());
    let daemon = boot(&repository);
    let (_nodes, launcher) = write_the_waiting_launch(&daemon);

    let mut launch = spawn_peppy(
        &daemon,
        &["stack", "launch", &launcher.display().to_string()],
    );
    let mut printed = PrintedLines::of(&mut launch);
    printed.wait_for("Validating dependencies", STEP_BOUND);
    poll_for(STEP_BOUND, "the launch to start its instance", || {
        lists_the_instance(&daemon).then_some(())
    });

    rustix::process::kill_process(
        rustix::process::Pid::from_child(&launch),
        rustix::process::Signal::INT,
    )
    .expect("press Ctrl-C");
    printed.wait_for(
        "Cancelling the launch; press Ctrl-C again to stop waiting for its end",
        STEP_BOUND,
    );
    let status = super::common::wait_for_exit(&mut launch, STEP_BOUND);
    let printed = printed.all();

    assert_eq!(
        status.code(),
        Some(130),
        "the command exits with 130: {printed}"
    );
    assert!(
        printed.contains("Launch was cancelled by its caller."),
        "the command says how the launch ended: {printed}"
    );
    poll_for(
        STEP_BOUND,
        "the cancel of the launch to clear the stack",
        || (!lists_the_instance(&daemon)).then_some(()),
    );
    let log = poll_for(STEP_BOUND, "the launch log to say why it ended", || {
        launch_log_holding(&daemon, "cancelled: cancelled by its caller")
    });
    assert!(log.exists());

    let (_home, _printed) = daemon.stop(DAEMON_EXIT);
}
