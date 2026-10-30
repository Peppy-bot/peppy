mod common;

use common::{
    GOAL_TIMEOUT, GoalEnd, NodeAddSource, RESULT_TIMEOUT, acquire_container_test_guard,
    archive_contains_entry, assert_failure, entity_artifact_path, path_env_with_dir_first,
    read_file_from_archive, send_logged_node_build, send_node_add_and_wait,
    send_node_build_and_wait, send_node_build_and_wait_forced, start_core_node_with_mock_messenger,
    start_core_node_with_mock_messenger_outside_home, write_executable_script, write_peppy_json5,
};
use config::failure_kind::PeppyFailureKind;
use core_node_api::encoding::{NodeBuildFeedback, NodeBuildResult};
use daemon_config::consts::DEFAULT_ALPINE_BASE_IMAGE;
use peppylib::messaging::Failure;
use std::path::Path;
use std::time::Duration;

const BUILD_CMD_MARKER_FILE: &str = "build_cmd_executed.marker";
const CONTAINER_RESULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Helper: stage a node via `send_node_add_and_wait` and assert success.
/// Returns (node_name, node_tag) for use in subsequent build calls.
async fn stage_node_for_build<'a>(
    started_core_node: &common::StartedCoreNode,
    source: impl Into<NodeAddSource<'a>>,
    result_timeout: Duration,
) -> (String, String) {
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source,
        GOAL_TIMEOUT,
        result_timeout,
        None,
    )
    .await
    .expect("node_add request should succeed");
    let added = add_result.completed();
    (added.node_name, added.node_tag)
}

/// Builds a `peppy.json5` document for a Rust node whose build step runs
/// `build_cmd`, given as a JSON5 array literal.
fn node_config_with_build_cmd(name: &str, tag: &str, build_cmd: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                description: ["A test node."],
            }},
            execution: {{
                language: "rust",
                build_cmd: {build_cmd},
                run_cmd: ["sleep", "10"]
            }}
        }}"#
    )
}

/// Builds a `peppy.json5` document for a Rust node built from the container
/// definition `apptainer.def` of its source directory.
fn container_node_config(name: &str, tag: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                description: ["A test node."],
            }},
            execution: {{
                language: "rust",
                container: {{
                    def_file: "apptainer.def",
                }}
            }}
        }}"#
    )
}

/// Builds the staged node `(node_name, node_tag)` with no env overrides and
/// no feedback channel, and returns how the build ended.
async fn build_node_and_wait(
    started_core_node: &common::StartedCoreNode,
    node_name: &str,
    node_tag: &str,
    result_timeout: Duration,
) -> GoalEnd<NodeBuildResult> {
    send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        node_name,
        node_tag,
        GOAL_TIMEOUT,
        result_timeout,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_runs_build_cmd() {
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(r#"["touch", "{BUILD_CMD_MARKER_FILE}"]"#),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result =
        build_node_and_wait(&started_core_node, &node_name, &node_tag, RESULT_TIMEOUT).await;

    let built = build_result.completed();

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(built.artifact_path.as_path(), archive_path.as_path());

    assert!(
        archive_contains_entry(&archive_path, BUILD_CMD_MARKER_FILE),
        "build_cmd should have created marker file in the archive"
    );

    let source_marker = source_dir.path().join(BUILD_CMD_MARKER_FILE);
    assert!(
        !source_marker.exists(),
        "build_cmd should NOT have created marker file in source dir at {}",
        source_marker.display()
    );
}

/// Only a node in `Added` is built. The core node is the root of the stack,
/// so the daemon refuses a build of it before anything runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_of_a_node_in_another_stage_than_added_is_rejected() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let build_result = build_node_and_wait(
        &started_core_node,
        &started_core_node.core_node_name,
        &started_core_node.core_node_tag,
        RESULT_TIMEOUT,
    )
    .await;

    assert_eq!(
        build_result.refusal(),
        &Failure::new(
            PeppyFailureKind::PreconditionFailed,
            format!(
                "node `{}:{}` is in stage `Root`; cannot build",
                started_core_node.core_node_name, started_core_node.core_node_tag
            )
        )
    );
}

/// The daemon refuses the build of a node in the stack without the working
/// dir `peppy node add` stages, and names the command that stages one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_of_a_node_without_a_staged_working_dir_is_rejected() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    write_peppy_json5(
        source_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "unstaged_node", tag: "v1", description: ["A test node."] },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"]
            }
        }"#,
    );
    let config = config::node::NodeConfigParser::from_path(source_dir.path().join("peppy.json5"))
        .expect("parse config");
    started_core_node
        .node_stack
        .push_config(config, Vec::new(), false, source_dir.path())
        .expect("push_config should succeed");

    let build_result =
        build_node_and_wait(&started_core_node, "unstaged_node", "v1", RESULT_TIMEOUT).await;

    assert_eq!(
        build_result.refusal(),
        &Failure::new(
            PeppyFailureKind::PreconditionFailed,
            "node `unstaged_node:v1` has no staged working directory; re-run `peppy node add` \
             to stage one"
        )
    );
}

/// The daemon hands the PyPI mirror of its config to every build: a Python
/// node build points the locked files the mirror has at it, keeps the other
/// one on PyPI, and says so in its log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_takes_the_pypi_mirror_of_the_daemon_config() {
    use httptest::matchers::request;
    use httptest::responders::status_code;
    use httptest::{Expectation, Server};

    const NODE_NAME: &str = "pypi_mirror_node";
    const NODE_TAG: &str = "v1";
    const PYPI_PACKAGES_BASE: &str = "https://files.pythonhosted.org/packages/";
    const ON_MIRROR: &str = "aa/bb/1f2e/on_mirror-1.0-py3-none-any.whl";
    const NOT_ON_MIRROR: &str = "cc/dd/3a4b/not_on_mirror-1.0-py3-none-any.whl";

    let mirror_server = Server::run();
    mirror_server.expect(
        Expectation::matching(request::method_path(
            "HEAD",
            format!("/packages/{ON_MIRROR}"),
        ))
        .respond_with(status_code(200).insert_header("content-length", "40")),
    );
    mirror_server.expect(
        Expectation::matching(request::method_path(
            "HEAD",
            format!("/packages/{NOT_ON_MIRROR}"),
        ))
        .respond_with(status_code(404)),
    );
    let mirror = daemon_config::peppy_config::PackagesBaseUrl::loopback_http_for_tests(
        &mirror_server.url_str("/packages/"),
    );
    let started_core_node = common::start_core_node_with_pypi_mirror(mirror.clone()).await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    write_peppy_json5(
        source_dir.path(),
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "{NODE_NAME}", tag: "{NODE_TAG}", description: ["A test node."] }},
                execution: {{
                    language: "python",
                    build_cmd: ["true"],
                    run_cmd: ["sleep", "10"]
                }}
            }}"#
        ),
    );
    let lock = format!(
        r#"version = 1
revision = 3
requires-python = ">=3.12"

[[package]]
name = "not-on-mirror"
version = "1.0"
source = {{ registry = "https://pypi.org/simple" }}
wheels = [
    {{ url = "{PYPI_PACKAGES_BASE}{NOT_ON_MIRROR}", hash = "sha256:11", size = 50 }},
]

[[package]]
name = "on-mirror"
version = "1.0"
source = {{ registry = "https://pypi.org/simple" }}
wheels = [
    {{ url = "{PYPI_PACKAGES_BASE}{ON_MIRROR}", hash = "sha256:22", size = 40 }},
]
"#
    );
    std::fs::write(source_dir.path().join("uv.lock"), &lock).expect("write uv.lock");

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;
    let build = send_logged_node_build(
        &started_core_node,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await;
    build.end.completed();

    let archive_path = entity_artifact_path(&started_core_node.node_stack, NODE_NAME, NODE_TAG);
    assert_eq!(
        read_file_from_archive(&archive_path, "uv.lock"),
        lock.replacen(
            &format!("{PYPI_PACKAGES_BASE}{ON_MIRROR}"),
            &format!("{}{ON_MIRROR}", mirror.as_str()),
            1
        )
    );
    let log = std::fs::read_to_string(&build.log_path).expect("read the build log");
    assert!(
        log.contains(&format!(
            "PyPI mirror: {}; uv.lock: 1 of 2 locked files from the mirror, 1 from files.pythonhosted.org",
            mirror.host()
        )),
        "the build log must say where the files come from, got:\n{log}"
    );
    assert_eq!(
        std::fs::read_to_string(source_dir.path().join("uv.lock")).expect("read the source lock"),
        lock,
        "the rewrite touches the staged copy only"
    );
}

/// Asserts that a build wrote its log `log_path` under `logs_dir_build()` as
/// `<name>_<tag>_*.log`, and returns the content of the log.
fn read_build_log(
    started_core_node: &common::StartedCoreNode,
    log_path: &Path,
    node_name: &str,
    node_tag: &str,
) -> String {
    assert!(
        !log_path.as_os_str().is_empty(),
        "log_path should not be empty"
    );
    assert!(log_path.exists(), "log file should exist at {log_path:?}");

    let log_dir = started_core_node.peppy_dirs.logs_dir_build();
    assert!(
        log_path.starts_with(&log_dir),
        "log file should be in logs_dir_build(), expected to start with {log_dir:?}, got {log_path:?}"
    );

    let log_filename = log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{node_name}_{node_tag}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );

    std::fs::read_to_string(log_path).expect("should be able to read log file")
}

/// Polls until `pid_file` exists and holds a PID, returning it.
async fn wait_for_pid_file(pid_file: &Path) -> u32 {
    common::poll_until(
        Duration::from_secs(30),
        "build_cmd did not record its PID within 30s",
        || {
            let contents = std::fs::read_to_string(pid_file).ok()?;
            contents.trim().parse::<u32>().ok()
        },
    )
    .await
}

/// Polls until the entity for `(name, tag)` is observably in `Building`.
async fn wait_for_building(node_stack: &node_stack::NodeStack, name: &str, tag: &str) {
    common::poll_until(
        Duration::from_secs(30),
        "entity did not enter Building within 30s",
        || {
            let handle = node_stack.find(name, tag)?;
            matches!(
                handle.read().stage(),
                node_stack::NodeStage::Building { .. }
            )
            .then_some(())
        },
    )
    .await
}

/// `node build --force` on a node whose build is stuck mid-flight (the daemon
/// task kept running after the CLI was Ctrl+C'd) must cancel + SIGKILL the
/// in-flight build, reuse the staged working dir, and succeed, rather than
/// rejecting with "is in stage `Building`; cannot build".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_build_cancels_inflight_and_reuses_working_dir_then_succeeds() {
    const TARGET_NODE_NAME: &str = "force_build_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Coordination files live outside the working dir so the test can read/touch
    // them regardless of where the build runs.
    let control_dir = tempfile::tempdir().expect("control tempdir");
    let pid_file = control_dir.path().join("build.pid");
    let proceed_file = control_dir.path().join("proceed");

    // The build_cmd records its PID, blocks on the proceed sentinel, then writes
    // the marker (relative path → lands in the working dir → ends up archived).
    let build_script = format!(
        "echo $$ > '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; touch '{}'",
        pid_file.display(),
        proceed_file.display(),
        BUILD_CMD_MARKER_FILE
    );

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(r#"["{build_script}"]"#),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    // The staged working dir (only surviving copy of the source) the forced
    // build must reuse.
    let staged_path = node_stack
        .find(&node_name, &node_tag)
        .expect("entity should exist after add")
        .read()
        .pending_working_dir()
        .map(|g| g.path().to_path_buf())
        .expect("a working dir should be staged after add");

    // Fire the first build; it blocks in `Building` on the proceed sentinel,
    // simulating a build whose CLI was Ctrl+C'd while the daemon keeps building.
    let first_build = {
        let messenger = started_core_node.caller_handle.clone();
        let core_node_name = started_core_node.core_node_name.clone();
        let node_name = node_name.clone();
        let node_tag = node_tag.clone();
        tokio::spawn(async move {
            send_node_build_and_wait(
                &messenger,
                &core_node_name,
                &node_name,
                &node_tag,
                GOAL_TIMEOUT,
                RESULT_TIMEOUT,
                Vec::new(),
                None,
            )
            .await
        })
    };

    let first_pid = wait_for_pid_file(&pid_file).await;
    wait_for_building(&node_stack, &node_name, &node_tag).await;

    // Force-rebuild: must cancel + SIGKILL the in-flight build and take over.
    let forced_build = {
        let messenger = started_core_node.caller_handle.clone();
        let core_node_name = started_core_node.core_node_name.clone();
        let node_name = node_name.clone();
        let node_tag = node_tag.clone();
        tokio::spawn(async move {
            send_node_build_and_wait_forced(
                &messenger,
                &core_node_name,
                &node_name,
                &node_tag,
                GOAL_TIMEOUT,
                RESULT_TIMEOUT,
                Vec::new(),
                None,
            )
            .await
        })
    };

    // The in-flight build's subprocess must actually be killed by the force.
    common::wait_until_process_exits(
        first_pid,
        Duration::from_secs(30),
        &format!("build subprocess (pid {first_pid}) was not killed within 30s"),
    )
    .await;

    // Let the (reused) forced build run to completion.
    std::fs::write(&proceed_file, b"").expect("touch proceed file");

    let forced_result = forced_build
        .await
        .expect("forced build task should not panic")
        .expect("forced build request should succeed");
    forced_result.completed();

    // The superseded first build ends cancelled, with the reason of the
    // forced build.
    let first_result = first_build
        .await
        .expect("first build task should not panic")
        .expect("first build request should resolve");
    assert_eq!(
        first_result.cancel_reason(),
        "superseded by a forced node build"
    );

    // The entity ends `Ready` and the archive proves the reused build_cmd ran to
    // completion.
    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert!(
        archive_contains_entry(&archive_path, BUILD_CMD_MARKER_FILE),
        "the forced build should have run build_cmd to completion in the reused working dir"
    );

    // The staged working dir was reused and then consumed by the successful
    // forced build (it was re-attached on cancel, not deleted).
    assert!(
        !staged_path.exists(),
        "the reused working dir should be cleaned up after a successful build"
    );
}

/// A `build_cmd` that cannot run, or that exits with a failure, fails the
/// build, and the error names the command.
#[rstest::rstest]
#[case::missing_command(
    r#"["this_command_does_not_exist_12345"]"#,
    "this_command_does_not_exist_12345"
)]
#[case::nonzero_exit(r#"["sh", "-c", "exit 1"]"#, "exit 1")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_fails_when_build_cmd_fails(
    #[case] build_cmd: &str,
    #[case] failed_command: &str,
) {
    const TARGET_NODE_NAME: &str = "add_cmd_fail_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(TARGET_NODE_NAME, TARGET_NODE_TAG, build_cmd);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result =
        build_node_and_wait(&started_core_node, &node_name, &node_tag, RESULT_TIMEOUT).await;

    let failure = build_result.failed();
    assert_failure(failure, PeppyFailureKind::AttemptFailed, "build_cmd failed");
    assert_failure(failure, PeppyFailureKind::AttemptFailed, failed_command);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_cmd_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "v1";
    const STDOUT_MARKER: &str = "peppy_add_stdout_marker";
    const STDERR_MARKER: &str = "peppy_add_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(r#"["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"]"#),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<NodeBuildFeedback>();
    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        Some(feedback_tx),
    )
    .await
    .expect("node_build request should succeed");

    build_result.completed();

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    let saw_stdout = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == STDOUT_MARKER);
    let saw_stderr = feedback
        .iter()
        .any(|entry| entry.is_stderr() && entry.line.trim() == STDERR_MARKER);

    assert!(saw_stdout, "stdout feedback should include marker");
    assert!(saw_stderr, "stderr feedback should include marker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "v1";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(r#"["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"]"#),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build = send_logged_node_build(
        &started_core_node,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await;

    build.end.completed();

    let log_content = read_build_log(
        &started_core_node,
        &build.log_path,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
    );

    assert!(
        log_content.contains(&format!("[stdout] {}", STDOUT_MARKER)),
        "log file should contain stdout marker with [stdout] prefix, got:
{}",
        log_content
    );
    assert!(
        log_content.contains(&format!("[stderr] {}", STDERR_MARKER)),
        "log file should contain stderr marker with [stderr] prefix, got:
{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_copies_files_to_storage() {
    const TARGET_NODE_NAME: &str = "copy_test_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let test_file_content = "test file content";
    std::fs::write(source_dir.path().join("test_file.txt"), test_file_content)
        .expect("failed to write test file");

    let sub_dir = source_dir.path().join("subdir");
    std::fs::create_dir(&sub_dir).expect("failed to create subdir");
    std::fs::write(sub_dir.join("nested_file.txt"), "nested content")
        .expect("failed to write nested file");

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                description: ["A test node."],
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result =
        build_node_and_wait(&started_core_node, &node_name, &node_tag, RESULT_TIMEOUT).await;

    let built = build_result.completed();

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(
        built.artifact_path.as_path(),
        archive_path.as_path(),
        "artifact_path should match archive path"
    );

    assert!(
        archive_contains_entry(&archive_path, "test_file.txt"),
        "test_file.txt should be in the archive"
    );
    let content = read_file_from_archive(&archive_path, "test_file.txt");
    assert_eq!(content, test_file_content, "file content should match");

    assert!(
        archive_contains_entry(&archive_path, "subdir/nested_file.txt"),
        "nested file should be in the archive"
    );
    let nested_content = read_file_from_archive(&archive_path, "subdir/nested_file.txt");
    assert_eq!(
        nested_content, "nested content",
        "nested content should match"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_uses_env_overrides_for_path() {
    const TARGET_NODE_NAME: &str = "the_node";
    const TARGET_NODE_TAG: &str = "v1";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(r#"["printout {STDOUT_MARKER}; printout {STDERR_MARKER} 1>&2"]"#),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    // First build attempt: should fail because `printout` is not on PATH.
    let build_result =
        build_node_and_wait(&started_core_node, &node_name, &node_tag, RESULT_TIMEOUT).await;
    assert_failure(
        build_result.failed(),
        PeppyFailureKind::AttemptFailed,
        "printout",
    );

    // Create a temp bin directory with a `printout` script.
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    write_executable_script(
        bin_dir.path(),
        "printout",
        "#!/bin/sh
echo \"$@\"
",
    );
    let env_vars = vec![path_env_with_dir_first(bin_dir.path())];

    // Re-stage the node and rebuild with the PATH override.
    stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        env_vars,
        None,
    )
    .await
    .expect("node_build request should succeed");

    build_result.completed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_injects_runtime_env_vars() {
    const TARGET_NODE_NAME: &str = "runtime_env_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &format!(
            r#"[
                "sh",
                "-c",
                "test -n \"$PEPPY_APPTAINER_BIN\" && test \"$PEPPY_NODE_NAME\" = \"{TARGET_NODE_NAME}\" && test \"$PEPPY_NODE_TAG\" = \"{TARGET_NODE_TAG}\""
            ]"#
        ),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result =
        build_node_and_wait(&started_core_node, &node_name, &node_tag, RESULT_TIMEOUT).await;

    build_result.completed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_with_container_success() {
    let _container_test_guard = acquire_container_test_guard().await;

    const TARGET_NODE_NAME: &str = "container_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = container_node_config(TARGET_NODE_NAME, TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);
    let apptainer_def = r#"
Bootstrap: docker
From: {DEFAULT_ALPINE_BASE_IMAGE}

%labels
    Name {TARGET_NODE_NAME}
    Version {TARGET_NODE_TAG}

%runscript
    echo "Running {TARGET_NODE_NAME}:{TARGET_NODE_TAG}"
"#
    .replace("{DEFAULT_ALPINE_BASE_IMAGE}", DEFAULT_ALPINE_BASE_IMAGE)
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");

    let (node_name, node_tag) = stage_node_for_build(
        &started_core_node,
        source_dir.path(),
        CONTAINER_RESULT_TIMEOUT,
    )
    .await;

    let build = send_logged_node_build(
        &started_core_node,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        None,
    )
    .await;

    let built = build.end.completed();

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));

    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(built.artifact_path.as_path(), root_path.as_path());
    assert!(
        root_path != source_dir.path(),
        "node should be stored in a different location than the source, got: {}",
        root_path.display()
    );
    assert!(
        root_path.exists(),
        "node .sif image should exist: {}",
        root_path.display()
    );

    assert_eq!(
        root_path.parent(),
        Some(
            started_core_node
                .peppy_dirs
                .built_node_dir(TARGET_NODE_NAME, TARGET_NODE_TAG)
                .as_path()
        ),
        "the image lives in the node identity's directory, got: {}",
        root_path.display()
    );
    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    let fingerprint = file_name
        .strip_suffix(".sif")
        .unwrap_or_else(|| panic!("stored image should end in .sif, got: {file_name}"));
    assert_eq!(
        fingerprint.len(),
        16,
        "the file stem is the 16-hex fingerprint of the staged tree, got: {fingerprint}"
    );
    assert!(
        fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "got: {fingerprint}"
    );

    let log_content = read_build_log(
        &started_core_node,
        &build.log_path,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
    );
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output with [stdout]/[stderr] prefixes, got:
{}",
        log_content
    );
}

/// Container builds must succeed when the daemon's peppy data root sits
/// OUTSIDE `$HOME`, as a `PEPPY_HOME` outside `$HOME` puts it (on macOS, for
/// example, under `/var/folders/…`).
///
/// On macOS the Lima guest VM only auto-mounts `$HOME`, so the build's
/// working dir (under `<root>/tmp`) is invisible to the guest unless the
/// build flow registers the root via `Apptainer::ensure_host_mounts`; without
/// it, command construction fails with "Path … is not accessible inside the
/// Lima VM". Every other starter roots the daemon under `test_tmp_root()`
/// (inside the repo, under `$HOME`), so only this test covers that
/// registration. On Linux the native backend imposes no mount restriction, so
/// there this test only exercises the plain build flow.
///
/// The def file deliberately copies a relative `%files` source (like the
/// generated `%files . /opt/{name}` defs): the build must run with the guest
/// cwd set to the working dir. Relying on `limactl shell`'s host-cwd
/// propagation breaks here: the kernel canonicalizes the cwd to
/// `/private/var/…` while the Lima mount only exists in the guest at the
/// `/var/folders/…` form, and Lima's failed `cd` is NON-fatal, so the build
/// would silently run from the home mount and `%files .` would copy the
/// user's entire `$HOME` into the image until the guest tmpfs filled up. The
/// facade `cd`s explicitly inside the guest wrapper and aborts if that fails;
/// a build from any other cwd fails this test with "cp: cannot stat
/// './marker.txt'".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_with_container_from_outside_home_root() {
    let _container_test_guard = acquire_container_test_guard().await;

    const TARGET_NODE_NAME: &str = "outside_home_container_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger_outside_home().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = container_node_config(TARGET_NODE_NAME, TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);
    let apptainer_def = format!(
        "Bootstrap: docker\n\
         From: {DEFAULT_ALPINE_BASE_IMAGE}\n\
         \n\
         %files\n\
         \x20   ./marker.txt /opt/marker.txt\n\
         \n\
         %runscript\n\
         \x20   echo \"Running {TARGET_NODE_NAME}:{TARGET_NODE_TAG}\"\n"
    );
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");
    std::fs::write(
        source_dir.path().join("marker.txt"),
        "guest cwd must be the build working dir\n",
    )
    .expect("failed to write marker file");

    let (node_name, node_tag) = stage_node_for_build(
        &started_core_node,
        source_dir.path(),
        CONTAINER_RESULT_TIMEOUT,
    )
    .await;

    let build_result = build_node_and_wait(
        &started_core_node,
        &node_name,
        &node_tag,
        CONTAINER_RESULT_TIMEOUT,
    )
    .await;

    // On macOS a build from a peppy root outside $HOME succeeds only when
    // the build flow registers the root as a Lima mount.
    build_result.completed();

    let artifact_path = entity_artifact_path(
        &started_core_node.node_stack,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
    );
    assert!(
        artifact_path.starts_with(started_core_node.peppy_dirs.root()),
        "artifact should be stored under the outside-home root, got: {}",
        artifact_path.display()
    );
    assert!(
        artifact_path.exists(),
        "node .sif image should exist: {}",
        artifact_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_container_build_failure_includes_stderr_in_error() {
    let _container_test_guard = acquire_container_test_guard().await;

    const TARGET_NODE_NAME: &str = "broken_container_node";
    const TARGET_NODE_TAG: &str = "v1";
    const INVALID_BOOTSTRAP_AGENT: &str = "invalid_bootstrap_agent_that_does_not_exist";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = container_node_config(TARGET_NODE_NAME, TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let broken_def = format!(
        "\
Bootstrap: {INVALID_BOOTSTRAP_AGENT}
From: nowhere

%runscript
    echo broken
"
    );
    std::fs::write(source_dir.path().join("apptainer.def"), broken_def)
        .expect("failed to write broken apptainer definition");

    let (node_name, node_tag) = stage_node_for_build(
        &started_core_node,
        source_dir.path(),
        CONTAINER_RESULT_TIMEOUT,
    )
    .await;

    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<NodeBuildFeedback>();
    let build = send_logged_node_build(
        &started_core_node,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        Some(&feedback_tx),
    )
    .await;

    let failure = build.end.failed();
    assert_failure(
        failure,
        PeppyFailureKind::AttemptFailed,
        "apptainer build failed",
    );
    assert!(
        failure.message().contains("stderr"),
        "error should include stderr output from apptainer build, got: {failure}"
    );
    assert!(
        failure.message().contains(INVALID_BOOTSTRAP_AGENT),
        "error should identify the invalid bootstrap agent, got: {failure}"
    );

    assert!(
        build.log_path.exists(),
        "log file should exist even on failure: {:?}",
        build.log_path
    );
    let log_content =
        std::fs::read_to_string(&build.log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output, got:
{}",
        log_content
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    assert!(
        !feedback.is_empty(),
        "feedback should have been streamed during the container build"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_logs_error_on_spawn_failure() {
    const TARGET_NODE_NAME: &str = "add_spawn_failure_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_build_cmd(
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        r#"["nonexistent_binary_peppy_test_xyz", "--flag"]"#,
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build = send_logged_node_build(
        &started_core_node,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await;

    let failure = build.end.failed();
    assert_failure(failure, PeppyFailureKind::AttemptFailed, "build_cmd failed");
    assert_failure(
        failure,
        PeppyFailureKind::AttemptFailed,
        "nonexistent_binary_peppy_test_xyz",
    );

    assert!(
        build.log_path.exists(),
        "log file should exist at {:?}",
        build.log_path
    );

    let log_content =
        std::fs::read_to_string(&build.log_path).expect("should be able to read log file");
    assert!(
        !log_content.is_empty(),
        "log file should not be empty when a spawn failure occurs"
    );
    assert!(
        log_content.contains("[error]"),
        "log file should contain an [error] entry, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("build_cmd failed"),
        "log file should contain the failure message, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("nonexistent_binary_peppy_test_xyz"),
        "log file should contain the command that failed, got:
{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_reset_cancels_a_running_build_and_allows_a_new_add() {
    let started = start_core_node_with_mock_messenger().await;
    let source = tempfile::tempdir().unwrap();
    let control = tempfile::tempdir().unwrap();
    let pid_file = control.path().join("build.pid");
    let build_script = format!("echo $$ > '{}'; exec sleep 300", pid_file.display());
    write_peppy_json5(
        source.path(),
        &format!(
            r#"{{
        peppy_schema: "node/v1",
        manifest: {{ name: "reset_build", tag: "v1", description: ["A test node."] }},
        execution: {{
            language: "rust",
            build_cmd: [{}],
            run_cmd: ["true"],
        }},
    }}"#,
            serde_json::to_string(&build_script).unwrap()
        ),
    );
    let (name, tag) = stage_node_for_build(&started, source.path(), RESULT_TIMEOUT).await;
    let build = {
        let messenger = started.caller_handle.clone();
        let core = started.core_node_name.clone();
        tokio::spawn(async move {
            send_node_build_and_wait(
                &messenger,
                &core,
                &name,
                &tag,
                GOAL_TIMEOUT,
                RESULT_TIMEOUT,
                Vec::new(),
                None,
            )
            .await
        })
    };
    let pid = wait_for_pid_file(&pid_file).await;
    // The poll panics on an error reply: the reset succeeds.
    common::poll_core_node_with_timeout(
        &started,
        &core_node_api::encoding::StackResetRequest::new(),
        RESULT_TIMEOUT,
    )
    .await;
    assert!(
        !common::is_process_running(pid),
        "reset must reap the build process"
    );
    assert_eq!(started.node_stack.len(), 1, "only the daemon remains");
    let build_end = build.await.unwrap().unwrap();
    assert_eq!(build_end.cancel_reason(), "cancelled by a stack reset");
    stage_node_for_build(&started, source.path(), RESULT_TIMEOUT).await;
}
