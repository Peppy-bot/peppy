//! Real-binary end-to-end coverage of the log export.
//!
//! Each case spawns the actual `peppy service serve` binary on a managed
//! zenohd, with `otlp_endpoint` in the config file of its own home, and runs
//! `peppy node` and `peppy stack` commands as second processes. The endpoint is an OTLP
//! receiver in the test process, a port nothing listens on, or a listener that
//! never answers. Assertions on what was exported run after the daemon has
//! exited, which is when it has sent everything it is going to send.
//!
//! Linux only: the managed zenohd is what lets a second process reach the
//! daemon.

#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod common;

use common::{
    Boot, DaemonGuard, MessagingEngine, PORT_ATTEMPTS, SERVE_INITIALIZED, free_port,
    spawn_daemon_with_env, wait_for_boot,
};
use daemon_config::consts::PeppyDirs;
use log_export::test_support::{OtlpReceiver, ReceivedRecord};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CORE_NODE_NAME: &str = "cn-export-e2e";
const NODE_NAME: &str = "exported_node";
const NODE_TAG: &str = "v1";
/// The value of the request header the daemon reads from its headers file.
const API_KEY: &str = "e2e-s3cr3t-api-key";
/// What the node's build prints: an info line, a warning and a line with no
/// level.
const BUILD_NOTE: &str = "INFO: fetching the deps of exported_node";
const BUILD_WARNING: &str = "warning: unused variable: `joint`";
const BUILD_STEP: &str = "Compiling exported_node v1";
/// What an instance of the node prints.
const RUN_LINE: &str = "exported_node is up";
/// A node whose build fails, and the error line it prints: the first line
/// `cargo build` prints for a type error.
const BROKEN_NODE_NAME: &str = "broken_node";
const CARGO_ERROR_LINE: &str = "error[E0308]: mismatched types";

/// How long a request of the running daemon gets to arrive: it leaves within
/// a second of its first record.
const REQUEST_ARRIVAL: Duration = Duration::from_secs(30);
/// How long the daemon gets to exit: its teardown and the flush of the export.
const DAEMON_EXIT: Duration = Duration::from_secs(30);

/// A daemon on its own home and zenoh port.
struct Daemon {
    home: tempfile::TempDir,
    port: u16,
    guard: DaemonGuard,
    logs: Arc<Mutex<String>>,
}

impl Daemon {
    /// Boots a daemon whose config file holds `otlp_settings` besides its
    /// core node name, and whose headers file holds [`API_KEY`].
    fn boot(otlp_settings: &str, envs: &[(&str, &str)]) -> Self {
        for _ in 0..PORT_ATTEMPTS {
            let home = tempfile::tempdir().expect("temp home");
            let dirs = PeppyDirs::new(home.path());
            std::fs::create_dir_all(dirs.conf_dir()).expect("create the conf dir");
            std::fs::write(
                dirs.conf_dir().join("peppy_config.json5"),
                format!("{{ core_node_name: \"{CORE_NODE_NAME}\", {otlp_settings} }}\n"),
            )
            .expect("write the config");
            std::fs::write(
                dirs.otlp_headers_path(),
                format!("{{ \"x-api-key\": \"{API_KEY}\" }}\n"),
            )
            .expect("write the headers file");

            let port = free_port();
            let (mut guard, logs) =
                spawn_daemon_with_env(home.path(), MessagingEngine::Zenoh { port }, envs);
            let boot = wait_for_boot(&mut guard, &logs, SERVE_INITIALIZED);
            if let Boot::Ready = boot {
                return Self {
                    home,
                    port,
                    guard,
                    logs,
                };
            }
        }
        panic!("{PORT_ATTEMPTS} attempts in a row hit a port collision");
    }

    fn dirs(&self) -> PeppyDirs {
        PeppyDirs::new(self.home.path())
    }

    /// Runs `peppy` with `args` against this daemon. Returns whether it
    /// succeeded and what it printed.
    fn peppy_outcome(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_peppy"))
            .args(args)
            .env(config::consts::PEPPY_HOME_ENV, self.home.path())
            .env(
                daemon_config::consts::PEPPY_MESSAGING_PORT_VAR_NAME,
                self.port.to_string(),
            )
            .env_remove("ZENOH_CONFIG")
            .env_remove(config::consts::PEPPY_CONFIG_ENV)
            .output()
            .expect("run peppy");
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), printed)
    }

    /// Runs `peppy` with `args` against this daemon, which succeeds. Returns
    /// what it printed.
    fn peppy(&self, args: &[&str]) -> String {
        let (success, printed) = self.peppy_outcome(args);
        assert!(
            success,
            "`peppy {}` failed:\n{printed}\nDaemon output:\n{}",
            args.join(" "),
            self.logs.lock().unwrap()
        );
        printed
    }

    /// Writes the node whose build prints [`BUILD_NOTE`], [`BUILD_WARNING`]
    /// and [`BUILD_STEP`], and whose instances print [`RUN_LINE`] and exit.
    fn write_the_node(&self) -> (tempfile::TempDir, PathBuf) {
        let nodes = tempfile::tempdir().expect("temp nodes dir");
        // A node directory is added by the binary that generated its code.
        let git_hash = common::read_daemon_git_hash(&self.home.path().join("daemon_state.json5"));
        let build = format!("echo '{BUILD_NOTE}'; echo '{BUILD_WARNING}'; echo '{BUILD_STEP}'");
        let run = format!("echo '{RUN_LINE}'");
        let manifest = format!(
            r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{NODE_NAME}", tag: "{NODE_TAG}" }},
            execution: {{
                language: "rust",
                build_cmd: ["sh", "-c", {build:?}],
                run_cmd: ["sh", "-c", {run:?}]
            }}
        }}"#
        );
        let node_dir = common::install_node_manifest(nodes.path(), NODE_NAME, &git_hash, &manifest);
        (nodes, node_dir)
    }

    /// Adds and builds the node of [`Self::write_the_node`].
    fn add_and_build_the_node(&self) {
        let (_nodes, node_dir) = self.write_the_node();
        self.peppy(&["node", "add", &node_dir.display().to_string(), "--build"]);
    }

    /// Adds and builds the node, runs an instance of it, and launches a stack
    /// of one more instance from a launcher that names the node. The add
    /// and the build succeed; the run and the launch end as the daemon
    /// decides for a node that exits as soon as it starts. Returns whether
    /// each of the four commands succeeded.
    fn add_build_run_and_launch_the_node(&self) -> [bool; 4] {
        let (nodes, node_dir) = self.write_the_node();
        let (added, _) = self.peppy_outcome(&["node", "add", &node_dir.display().to_string()]);
        let (built, _) = self.peppy_outcome(&["node", "build", &format!("{NODE_NAME}:{NODE_TAG}")]);
        let (ran, _) = self.peppy_outcome(&[
            "node",
            "run",
            &format!("{NODE_NAME}:{NODE_TAG}"),
            "--instance-id",
            "ran_inst",
        ]);
        common::register_repo_caches(self.home.path(), &[(NODE_NAME, NODE_TAG, &node_dir)]);
        let launcher = nodes.path().join("peppy_launcher.json5");
        std::fs::write(
            &launcher,
            format!(
                r#"{{ peppy_schema: "launcher/v1", deployments: [ {{ source: {{ name: "{NODE_NAME}:{NODE_TAG}" }}, instances: [ {{ instance_id: "launched_inst" }} ] }} ] }}"#
            ),
        )
        .expect("write the launcher");
        let (launched, _) =
            self.peppy_outcome(&["stack", "launch", &launcher.display().to_string()]);
        [added, built, ran, launched]
    }

    /// Adds a node whose build prints the error line of a `cargo build` that
    /// fails on a type error, and fails. The add fails.
    fn add_the_broken_node(&self) {
        let nodes = tempfile::tempdir().expect("temp nodes dir");
        let git_hash = common::read_daemon_git_hash(&self.home.path().join("daemon_state.json5"));
        let build = format!("echo '{CARGO_ERROR_LINE}' >&2; exit 101");
        let manifest = format!(
            r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{BROKEN_NODE_NAME}", tag: "{NODE_TAG}" }},
            execution: {{
                language: "rust",
                build_cmd: ["sh", "-c", {build:?}],
                run_cmd: ["sh", "-c", "exit 0"]
            }}
        }}"#
        );
        let node_dir =
            common::install_node_manifest(nodes.path(), BROKEN_NODE_NAME, &git_hash, &manifest);
        let (success, _) =
            self.peppy_outcome(&["node", "add", &node_dir.display().to_string(), "--build"]);
        assert!(!success, "a failed build fails the add");
    }

    /// Stops the daemon with SIGTERM and waits for it to exit. Returns what
    /// it printed.
    fn stop(mut self) -> (tempfile::TempDir, String) {
        let pid = rustix::process::Pid::from_child(&self.guard.0);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM)
            .expect("signal the daemon");
        let status = common::wait_for_exit(&mut self.guard.0, DAEMON_EXIT);
        let printed = self.logs.lock().unwrap().clone();
        assert!(status.success(), "the daemon exits cleanly:\n{printed}");
        (self.home, printed)
    }
}

/// The log files under `dir`, oldest first.
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{} should exist: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    files.sort();
    files
}

/// The entries of the log at `path`.
fn lines_of(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()));
    node_stack::action_log::test_support::log_entries(&content)
}

/// The one log file under `dir`.
fn only_log(dir: &Path) -> PathBuf {
    let [log]: [PathBuf; 1] = log_files(dir)
        .try_into()
        .unwrap_or_else(|logs| panic!("one log under {}: {logs:?}", dir.display()));
    log
}

/// The bodies of the records exported from the log at `path`, in order.
fn bodies_from<'a>(records: &'a [ReceivedRecord], path: &Path) -> Vec<&'a str> {
    let path = path.display().to_string();
    records
        .iter()
        .filter(|record| record.attributes.get("log.file.path") == Some(&path))
        .map(|record| record.body.as_str())
        .collect()
}

fn receiver(runtime: &tokio::runtime::Runtime) -> OtlpReceiver {
    runtime.block_on(OtlpReceiver::start("127.0.0.1:0"))
}

/// The port OTLP/HTTP receivers listen on by default.
const OTLP_DEFAULT_PORT: u16 = 4318;

/// A receiver on the default OTLP port, or on a free port when another
/// process listens there, in which case the test cannot tell a daemon that
/// posts to the default port from one that posts nowhere.
fn receiver_on_the_default_port(runtime: &tokio::runtime::Runtime) -> OtlpReceiver {
    let receiver = runtime.block_on(OtlpReceiver::start_or_free(&format!(
        "127.0.0.1:{OTLP_DEFAULT_PORT}"
    )));
    if receiver.port() != OTLP_DEFAULT_PORT {
        eprintln!(
            "port {OTLP_DEFAULT_PORT} is taken; the receiver listens on {}",
            receiver.port()
        );
    }
    receiver
}

#[test]
fn a_daemon_exports_each_line_of_its_add_and_build_logs() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    let daemon = Daemon::boot(&format!("otlp_endpoint: \"{}\"", receiver.endpoint()), &[]);
    let dirs = daemon.dirs();

    daemon.add_and_build_the_node();
    let (home, printed) = daemon.stop();

    let requests = receiver.take_requests();
    assert!(!requests.is_empty(), "the daemon exported:\n{printed}");
    for request in &requests {
        assert_eq!(request.path, "/v1/logs");
        assert_eq!(request.headers["x-api-key"], API_KEY);
        assert_eq!(request.headers["content-type"], "application/x-protobuf");
    }
    let records: Vec<ReceivedRecord> = requests
        .into_iter()
        .flat_map(|request| request.records)
        .collect();

    // One record for each line of each log file, in the order of the file.
    let add_log = only_log(&dirs.logs_dir_add());
    let build_log = only_log(&dirs.logs_dir_build());
    for log in [&add_log, &build_log] {
        let lines = lines_of(log);
        assert!(!lines.is_empty(), "{} holds lines", log.display());
        assert_eq!(bodies_from(&records, log), lines, "{}", log.display());
    }
    assert_eq!(
        records.len(),
        lines_of(&add_log).len() + lines_of(&build_log).len(),
        "nothing but the two logs is exported: {records:#?}"
    );

    for record in &records {
        assert_eq!(record.resource["service.name"], NODE_NAME);
        assert_eq!(record.resource["service.namespace"], CORE_NODE_NAME);
        assert_eq!(record.resource["peppy.core_node.name"], CORE_NODE_NAME);
        assert_eq!(record.resource["host.name"], core_node::current_host_name());
        assert_eq!(
            record.resource["peppy.version"],
            daemon_config::consts::PEPPY_VERSION
        );
        assert_eq!(record.attributes["peppy.node.name"], NODE_NAME);
        assert_eq!(record.attributes["peppy.node.tag"], NODE_TAG);
        assert_ne!(record.observed_time_unix_nano, 0);
    }
    let build: Vec<&ReceivedRecord> = records
        .iter()
        .filter(|record| record.attributes["peppy.log"] == "build")
        .collect();
    let warning = build
        .iter()
        .find(|record| record.body == BUILD_WARNING)
        .expect("the build's warning is exported");
    assert_eq!(
        (warning.severity_number, warning.severity_text.as_str()),
        (13, "warning")
    );
    assert_eq!(warning.attributes["log.iostream"], "stdout");
    assert_eq!(warning.time_unix_nano, 0);
    let step = build
        .iter()
        .find(|record| record.body == BUILD_STEP)
        .expect("the build's step is exported");
    assert_eq!((step.severity_number, step.severity_text.as_str()), (0, ""));

    // The headers file is the owner's alone, and its value is in the request
    // headers only.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dirs.otlp_headers_path())
            .expect("the headers file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(!printed.contains(API_KEY), "the daemon printed the key");
    for log in [&add_log, &build_log, &dirs.stack_log_path()] {
        let content = std::fs::read_to_string(log).unwrap_or_default();
        assert!(
            !content.contains(API_KEY),
            "{} holds the key",
            log.display()
        );
    }
    drop(home);
}

/// The entries of each log file under `dirs`, by the kind of the log, with
/// what differs between two daemons masked: the daemon's home, the temporary
/// directories, and the fingerprints and ids the daemon draws. The logs of
/// a kind are sorted, since their names carry the time they were created.
fn entries_of_every_log(dirs: &PeppyDirs) -> BTreeMap<&'static str, Vec<Vec<String>>> {
    let home = dirs.root().display().to_string();
    let masked = |line: &str| -> String {
        let line = mask_hex_runs(&line.replace(&home, "<home>"));
        line.split(' ')
            .map(|word| match word.find("/tmp/.tmp") {
                Some(start) => format!("{}<tmp>", &word[..start]),
                None => word.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    [
        ("add", dirs.logs_dir_add()),
        ("build", dirs.logs_dir_build()),
        ("run", dirs.logs_dir_run()),
        ("launch", dirs.logs_dir_launch()),
    ]
    .into_iter()
    .map(|(kind, dir)| {
        let mut logs: Vec<Vec<String>> = log_files(&dir)
            .iter()
            .map(|log| lines_of(log).iter().map(|line| masked(line)).collect())
            .collect();
        logs.sort();
        (kind, logs)
    })
    .collect()
}

/// `line` with every run of 16 or more hex digits, such as a fingerprint,
/// replaced by `<hex>`.
fn mask_hex_runs(line: &str) -> String {
    let mut masked = String::with_capacity(line.len());
    let mut run = String::new();
    for c in line.chars().chain(std::iter::once(' ')) {
        if (c.is_ascii_hexdigit() && c.is_ascii_lowercase()) || c.is_ascii_digit() {
            run.push(c);
            continue;
        }
        if run.len() >= 16 {
            masked.push_str("<hex>");
        } else {
            masked.push_str(&run);
        }
        run.clear();
        masked.push(c);
    }
    masked.pop();
    masked
}

/// Adds, builds, runs and launches the node on a daemon that exports to
/// `endpoint`, which takes no request, and on a daemon with the export off,
/// and stops both. The add and the build succeed on both; the run and the
/// launch end the same way on both, and the daemons exit. The log files of
/// the two daemons hold the same entries.
fn assert_every_command_takes_its_course_without(endpoint: &str) {
    let exporting = Daemon::boot(&format!("otlp_endpoint: \"{endpoint}\""), &[]);
    let exporting_dirs = exporting.dirs();
    let exporting_outcomes = exporting.add_build_run_and_launch_the_node();
    let (_home, _printed) = exporting.stop();

    let off = Daemon::boot("otlp_endpoint: null", &[]);
    let off_dirs = off.dirs();
    let off_outcomes = off.add_build_run_and_launch_the_node();
    let (_off_home, _off_printed) = off.stop();

    assert_eq!(
        &off_outcomes[..2],
        [true, true],
        "the add and the build succeed"
    );
    assert_eq!(exporting_outcomes, off_outcomes);

    let exported = entries_of_every_log(&exporting_dirs);
    let kept = entries_of_every_log(&off_dirs);
    for (kind, logs) in &kept {
        assert_eq!(&exported[kind], logs, "the {kind} logs");
    }
    for expected in [BUILD_WARNING, BUILD_STEP, RUN_LINE] {
        let kind = if expected == RUN_LINE { "run" } else { "build" };
        assert!(
            kept[kind].iter().flatten().any(|line| line == expected),
            "`{expected}` is in a {kind} log: {:#?}",
            kept[kind]
        );
    }
}

#[test]
fn every_command_takes_its_course_while_nothing_listens_at_the_endpoint() {
    assert_every_command_takes_its_course_without(&format!("http://127.0.0.1:{}", free_port()));
}

#[test]
fn every_command_takes_its_course_while_the_endpoint_never_answers() {
    // Accepts each connection and never answers on it.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the silent endpoint");
    let endpoint = format!("http://{}", listener.local_addr().expect("local address"));
    std::thread::spawn(move || {
        let open: Vec<_> = listener.incoming().collect();
        drop(open);
    });
    assert_every_command_takes_its_course_without(&endpoint);
}

/// The peppy binary logs through `tracing`; a debug build prints its verbose
/// style with colors. Run as a node, its lines are exported with the level
/// they print.
#[test]
fn a_rust_node_that_logs_through_tracing_exports_the_levels_it_prints() {
    const TRACING_NODE_NAME: &str = "tracing_node";
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    let daemon = Daemon::boot(&format!("otlp_endpoint: \"{}\"", receiver.endpoint()), &[]);
    let nodes = tempfile::tempdir().expect("temp nodes dir");
    let git_hash = common::read_daemon_git_hash(&daemon.home.path().join("daemon_state.json5"));
    // `peppy node info` of a node the daemon does not hold logs an info line
    // on its way and an error line for the answer.
    let manifest = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{TRACING_NODE_NAME}", tag: "{NODE_TAG}" }},
            execution: {{
                language: "rust",
                build_cmd: ["true"],
                run_cmd: [{peppy:?}, "node", "info", "no_such_node:v1"]
            }}
        }}"#,
        peppy = env!("CARGO_BIN_EXE_peppy"),
    );
    let node_dir =
        common::install_node_manifest(nodes.path(), TRACING_NODE_NAME, &git_hash, &manifest);
    daemon.peppy(&["node", "add", &node_dir.display().to_string(), "--build"]);
    let (_, _) = daemon.peppy_outcome(&[
        "node",
        "run",
        &format!("{TRACING_NODE_NAME}:{NODE_TAG}"),
        "--instance-id",
        "tracing_inst",
    ]);
    let (_home, _printed) = daemon.stop();

    let records: Vec<ReceivedRecord> = receiver
        .take_records()
        .into_iter()
        .filter(|record| {
            record.attributes["peppy.log"] == "run"
                && record.attributes.contains_key("log.iostream")
        })
        .collect();
    assert!(!records.is_empty(), "the node's output is exported");
    for level in ["INFO", "ERROR"] {
        let record = records
            .iter()
            .find(|record| record.severity_text == level)
            .unwrap_or_else(|| panic!("a {level} line is exported: {records:#?}"));
        assert_eq!(record.severity_number, if level == "INFO" { 9 } else { 17 });
        assert!(!record.body.contains('\u{1b}'), "{}", record.body);
        assert!(record.body.contains(level), "{}", record.body);
    }
}

/// Python's `logging` prints `LEVEL:name:message` by default. A node that
/// logs through it exports the levels it prints, and a `print` a record with
/// no severity.
#[test]
fn a_python_node_that_logs_through_logging_exports_the_levels_it_prints() {
    const LOGGING_NODE_NAME: &str = "logging_node";
    const PRINTED_LINE: &str = "logging_node printed a line";
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    let daemon = Daemon::boot(&format!("otlp_endpoint: \"{}\"", receiver.endpoint()), &[]);
    let nodes = tempfile::tempdir().expect("temp nodes dir");
    let git_hash = common::read_daemon_git_hash(&daemon.home.path().join("daemon_state.json5"));
    let program = format!(
        "import logging; logging.warning('disk is nearly full'); \
         logging.error('motor fault'); print('{PRINTED_LINE}', flush=True)"
    );
    let manifest = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{ name: "{LOGGING_NODE_NAME}", tag: "{NODE_TAG}" }},
            execution: {{
                language: "python",
                build_cmd: ["true"],
                run_cmd: ["python3", "-c", {program:?}]
            }}
        }}"#,
    );
    let node_dir =
        common::install_node_manifest(nodes.path(), LOGGING_NODE_NAME, &git_hash, &manifest);
    daemon.peppy(&["node", "add", &node_dir.display().to_string(), "--build"]);
    let (_, _) = daemon.peppy_outcome(&[
        "node",
        "run",
        &format!("{LOGGING_NODE_NAME}:{NODE_TAG}"),
        "--instance-id",
        "logging_inst",
    ]);
    let (_home, _printed) = daemon.stop();

    let records: Vec<ReceivedRecord> = receiver
        .take_records()
        .into_iter()
        .filter(|record| {
            record.attributes["peppy.log"] == "run"
                && record.attributes.contains_key("log.iostream")
        })
        .collect();
    for (level, number, message) in [
        ("WARNING", 13, "disk is nearly full"),
        ("ERROR", 17, "motor fault"),
    ] {
        let record = records
            .iter()
            .find(|record| record.severity_text == level)
            .unwrap_or_else(|| panic!("a {level} line is exported: {records:#?}"));
        assert_eq!(record.severity_number, number);
        assert!(record.body.contains(message), "{}", record.body);
    }
    let printed = records
        .iter()
        .find(|record| record.body == PRINTED_LINE)
        .unwrap_or_else(|| panic!("the printed line is exported: {records:#?}"));
    assert_eq!(printed.severity_number, 0);
    assert_eq!(printed.severity_text, "");
}

/// A failed build exports an error record for the `error:` line it prints.
#[test]
fn a_failed_build_exports_an_error_record_for_its_error_line() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    let daemon = Daemon::boot(&format!("otlp_endpoint: \"{}\"", receiver.endpoint()), &[]);

    daemon.add_the_broken_node();
    let (_home, _printed) = daemon.stop();

    let records = receiver.take_records();
    let error = records
        .iter()
        .find(|record| record.body == CARGO_ERROR_LINE)
        .unwrap_or_else(|| panic!("the error line is exported: {records:#?}"));
    assert_eq!(
        (error.severity_number, error.severity_text.as_str()),
        (17, "error")
    );
    assert_eq!(error.attributes["peppy.log"], "build");
    assert_eq!(error.attributes["log.iostream"], "stderr");
    assert_eq!(error.resource["service.name"], BROKEN_NODE_NAME);
    // The build failure the daemon writes is an error record too.
    let failure = records
        .iter()
        .find(|record| record.body.starts_with("Failed to build node"))
        .unwrap_or_else(|| panic!("the build failure is exported: {records:#?}"));
    assert_eq!(failure.severity_number, 17);
}

#[test]
fn a_daemon_with_no_endpoint_sends_nothing() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver_on_the_default_port(&runtime);
    let endpoint = receiver.endpoint();
    // The settings are absent from the file: the daemon completes them. The
    // OpenTelemetry environment variables name the receiver, and the daemon
    // reads none of them.
    let daemon = Daemon::boot(
        "lifecycle: {}",
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", endpoint.as_str()),
        ],
    );
    let dirs = daemon.dirs();

    daemon.add_and_build_the_node();
    let (_home, printed) = daemon.stop();

    let config = std::fs::read_to_string(dirs.conf_dir().join("peppy_config.json5"))
        .expect("the completed config");
    assert!(config.contains("  otlp_endpoint: null,\n"), "{config}");
    assert!(config.contains("  otlp_min_severity: null,\n"), "{config}");
    assert!(
        receiver.take_requests().is_empty(),
        "a daemon with no endpoint sent a request:\n{printed}"
    );
}

#[test]
fn a_line_below_the_minimum_severity_stays_in_its_file() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    let daemon = Daemon::boot(
        &format!(
            "otlp_endpoint: \"{}\", otlp_min_severity: \"warn\"",
            receiver.endpoint()
        ),
        &[],
    );
    let dirs = daemon.dirs();

    daemon.add_and_build_the_node();
    let (_home, _printed) = daemon.stop();

    let build_log = only_log(&dirs.logs_dir_build());
    let lines = lines_of(&build_log);
    assert!(lines.iter().any(|line| line == BUILD_NOTE), "{lines:#?}");
    let records = receiver.take_records();
    let exported = bodies_from(&records, &build_log);
    // The info line is below the minimum severity; the warning is at it, and
    // the step prints no level.
    assert!(!exported.contains(&BUILD_NOTE), "{exported:#?}");
    assert!(exported.contains(&BUILD_WARNING), "{exported:#?}");
    assert!(exported.contains(&BUILD_STEP), "{exported:#?}");
    const WARN_NUMBER: i32 = 13;
    assert!(
        records
            .iter()
            .all(|record| record.severity_number == 0 || record.severity_number >= WARN_NUMBER),
        "only lines at warn and above, or with no level, are exported: {records:#?}"
    );
}

#[test]
fn a_rejected_key_is_read_again_from_the_headers_file() {
    const ROTATED_KEY: &str = "e2e-rotated-api-key";
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let mut receiver = receiver(&runtime);
    receiver.answer_next([401]);
    let daemon = Daemon::boot(&format!("otlp_endpoint: \"{}\"", receiver.endpoint()), &[]);
    // The daemon read the first key when it started.
    std::fs::write(
        daemon.dirs().otlp_headers_path(),
        format!("{{ \"x-api-key\": \"{ROTATED_KEY}\" }}\n"),
    )
    .expect("rotate the key");

    daemon.add_and_build_the_node();
    // The rejected request and the one that follows it, which go out while
    // the daemon runs: a daemon that stops sends each request once.
    let mut requests = Vec::new();
    for _ in 0..2 {
        let request = runtime
            .block_on(async {
                tokio::time::timeout(REQUEST_ARRIVAL, receiver.next_request()).await
            })
            .expect("a request arrives");
        requests.push(request);
    }
    let (_home, printed) = daemon.stop();
    requests.extend(receiver.take_requests());

    let keys: Vec<(&str, u16)> = requests
        .iter()
        .map(|request| (request.headers["x-api-key"].as_str(), request.answered))
        .collect();
    assert_eq!(keys[0], (API_KEY, 401), "{keys:?}\n{printed}");
    assert!(
        keys[1..].iter().all(|key| *key == (ROTATED_KEY, 200)),
        "{keys:?}"
    );
    // The request that was rejected is the one sent again.
    assert_eq!(requests[0].records, requests[1].records);
}
