mod common;

use common::{
    NodeAddSource, send_node_add_and_wait, send_node_add_and_wait_with_env,
    send_node_build_and_wait, start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::consts::DEFAULT_ALPINE_BASE_IMAGE;
use core_node::encoding::NodeBuildFeedback;
use std::path::{Path, PathBuf};
use std::time::Duration;

const BUILD_CMD_MARKER_FILE: &str = "build_cmd_executed.marker";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);
const CONTAINER_RESULT_TIMEOUT: Duration = Duration::from_secs(300);

fn entity_artifact_path(node_stack: &node_stack::NodeStack, name: &str, tag: &str) -> PathBuf {
    node_stack
        .find(name, tag)
        .expect("entity should exist")
        .read()
        .artifact_path()
        .expect("entity should be built")
        .to_path_buf()
}

fn archive_contains_entry(archive_path: &Path, entry_name: &str) -> bool {
    let file = std::fs::File::open(archive_path).expect("failed to open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .expect("failed to read entries")
        .any(|entry| {
            let entry = entry.expect("failed to read entry");
            let path = entry.path().expect("failed to read entry path");
            let path_str = path.to_string_lossy();
            let normalized = path_str.trim_start_matches("./");
            normalized == entry_name
        })
}

fn read_file_from_archive(archive_path: &Path, entry_name: &str) -> String {
    use std::io::Read;
    let file = std::fs::File::open(archive_path).expect("failed to open archive");
    let decoder = zstd::stream::read::Decoder::new(file).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().expect("failed to read entries") {
        let mut entry = entry.expect("failed to read entry");
        let path = entry.path().expect("failed to read entry path");
        let path_str = path.to_string_lossy();
        let normalized = path_str.trim_start_matches("./").to_string();
        if normalized == entry_name {
            let mut contents = String::new();
            entry
                .read_to_string(&mut contents)
                .expect("failed to read entry contents");
            return contents;
        }
    }
    panic!(
        "entry '{}' not found in archive {}",
        entry_name,
        archive_path.display()
    );
}

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
    assert!(
        add_result.success,
        "node_add should succeed before build, got error: {:?}",
        add_result.error_message
    );
    (
        add_result.node_name.expect("node_name on add success"),
        add_result.node_tag.expect("node_tag on add success"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_runs_build_cmd() {
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["touch", "{BUILD_CMD_MARKER_FILE}"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{BUILD_CMD_MARKER_FILE}", BUILD_CMD_MARKER_FILE);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should succeed");

    assert!(
        build_result.success,
        "node_build should succeed, got error: {:?}",
        build_result.error_message
    );

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(build_result.artifact_path.as_path(), archive_path.as_path());

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_cmd_failure_fails_build() {
    const TARGET_NODE_NAME: &str = "add_cmd_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["this_command_does_not_exist_12345"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");

    assert!(
        !build_result.success,
        "node_build should fail when build_cmd fails"
    );
    assert!(
        build_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("build_cmd failed"))
            .unwrap_or(false),
        "error message should mention build_cmd failure, got: {:?}",
        build_result.error_message
    );
    assert!(
        build_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("this_command_does_not_exist_12345"))
            .unwrap_or(false),
        "error message should include the command that failed, got: {:?}",
        build_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_cmd_nonzero_exit_fails_build() {
    const TARGET_NODE_NAME: &str = "add_cmd_exit_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["sh", "-c", "exit 1"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");

    assert!(
        !build_result.success,
        "node_build should fail when build_cmd exits with non-zero status"
    );
    assert!(
        build_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("build_cmd failed"))
            .unwrap_or(false),
        "error message should mention build_cmd failure, got: {:?}",
        build_result.error_message
    );
    assert!(
        build_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("exit 1"))
            .unwrap_or(false),
        "error message should include the command that failed, got: {:?}",
        build_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_cmd_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_add_stdout_marker";
    const STDERR_MARKER: &str = "peppy_add_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
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

    assert!(
        build_result.success,
        "node_build should succeed, got error: {:?}",
        build_result.error_message
    );

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
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should succeed");

    assert!(
        build_result.success,
        "node_build should succeed, got error: {:?}",
        build_result.error_message
    );

    assert!(
        !build_result.log_path.as_os_str().is_empty(),
        "log_path should not be empty"
    );
    assert!(
        build_result.log_path.exists(),
        "log file should exist at {:?}",
        build_result.log_path
    );

    let log_dir = started_core_node.peppy_dirs.logs_dir_build();
    assert!(
        build_result.log_path.starts_with(&log_dir),
        "log file should be in logs_dir_build(), expected to start with {:?}, got {:?}",
        log_dir,
        build_result.log_path
    );

    let log_filename = build_result
        .log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );

    let log_content =
        std::fs::read_to_string(&build_result.log_path).expect("should be able to read log file");

    assert!(
        log_content.contains(&format!("[stdout] {}", STDOUT_MARKER)),
        "log file should contain stdout marker with [stdout] prefix, got:\n{}",
        log_content
    );
    assert!(
        log_content.contains(&format!("[stderr] {}", STDERR_MARKER)),
        "log file should contain stderr marker with [stderr] prefix, got:\n{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_copies_files_to_storage() {
    const TARGET_NODE_NAME: &str = "copy_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

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
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should succeed");

    assert!(
        build_result.success,
        "node_build should succeed, got error: {:?}",
        build_result.error_message
    );

    let archive_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(
        build_result.artifact_path.as_path(),
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
    const TARGET_NODE_TAG: &str = "0.1.0";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["printout {STDOUT_MARKER}; printout {STDERR_MARKER} 1>&2"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    // First build attempt: should fail because `printout` is not on PATH.
    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");
    assert!(
        !build_result.success,
        "build should fail, printout does not exist: {:?}",
        build_result.error_message
    );

    // Create a temp bin directory with a `printout` script.
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    let printout_path = bin_dir.path().join("printout");
    std::fs::write(&printout_path, "#!/bin/sh\necho \"$@\"\n")
        .expect("failed to write printout script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&printout_path)
            .expect("failed to get printout metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&printout_path, perms)
            .expect("failed to set printout permissions");
    }

    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.path().display(), current_path);
    let env_vars = vec![("PATH".to_string(), new_path)];

    // Re-stage the node and rebuild with the PATH override.
    let _ = send_node_add_and_wait_with_env(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
        Vec::new(),
    )
    .await
    .expect("re-add should complete");

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

    assert!(
        build_result.success,
        "build should succeed with PATH override, got error: {:?}",
        build_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_injects_runtime_env_vars() {
    const TARGET_NODE_NAME: &str = "runtime_env_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: [
                    "sh",
                    "-c",
                    "test -n \"$PEPPY_APPTAINER_BIN\" && test \"$PEPPY_NODE_NAME\" = \"{TARGET_NODE_NAME}\" && test \"$PEPPY_NODE_TAG\" = \"{TARGET_NODE_TAG}\""
                ],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should succeed");

    assert!(
        build_result.success,
        "node_build should succeed when runtime env vars are injected, got error: {:?}",
        build_result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_with_container_success() {
    const TARGET_NODE_NAME: &str = "container_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
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

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should succeed");

    assert!(
        build_result.success,
        "node_build should succeed, got error: {:?}",
        build_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));

    let root_path = entity_artifact_path(&node_stack, TARGET_NODE_NAME, TARGET_NODE_TAG);
    assert_eq!(build_result.artifact_path.as_path(), root_path.as_path());
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

    let file_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have file name");
    assert_eq!(
        file_name,
        format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}.sif"),
        "stored image should be '<node_name>_<tag>.sif', got: {}",
        file_name
    );

    assert!(
        !build_result.log_path.as_os_str().is_empty(),
        "log_path should not be empty"
    );
    assert!(
        build_result.log_path.exists(),
        "log file should exist at {:?}",
        build_result.log_path
    );

    let log_dir = started_core_node.peppy_dirs.logs_dir_build();
    assert!(
        build_result.log_path.starts_with(&log_dir),
        "log file should be in logs_dir_build(), expected to start with {:?}, got {:?}",
        log_dir,
        build_result.log_path
    );

    let log_filename = build_result
        .log_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have log filename");
    assert!(
        log_filename.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "log filename should start with '<node_name>_<tag>_', got: {}",
        log_filename
    );
    assert!(
        log_filename.ends_with(".log"),
        "log filename should end with '.log', got: {}",
        log_filename
    );

    let log_content =
        std::fs::read_to_string(&build_result.log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output with [stdout]/[stderr] prefixes, got:\n{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_build_container_build_failure_includes_stderr_in_error() {
    const TARGET_NODE_NAME: &str = "broken_container_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let broken_def = "\
Bootstrap: invalid_bootstrap_agent_that_does_not_exist
From: nowhere

%runscript
    echo broken
";
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
    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        CONTAINER_RESULT_TIMEOUT,
        Vec::new(),
        Some(feedback_tx),
    )
    .await
    .expect("node_build request should complete");

    assert!(
        !build_result.success,
        "node_build should fail with a broken def file"
    );

    let error_msg = build_result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains("apptainer build failed"),
        "error should mention apptainer build failure, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("stderr"),
        "error should include stderr output from apptainer build, got: {}",
        error_msg
    );

    assert!(
        build_result.log_path.exists(),
        "log file should exist even on failure: {:?}",
        build_result.log_path
    );
    let log_content =
        std::fs::read_to_string(&build_result.log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("[stdout]") || log_content.contains("[stderr]"),
        "log file should contain streamed build output, got:\n{}",
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
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                build_cmd: ["nonexistent_binary_peppy_test_xyz", "--flag"],
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let (node_name, node_tag) =
        stage_node_for_build(&started_core_node, source_dir.path(), RESULT_TIMEOUT).await;

    let build_result = send_node_build_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");

    assert!(
        !build_result.success,
        "node_build should fail when spawn fails"
    );

    let error_msg = build_result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains("build_cmd failed"),
        "error should mention build_cmd failure, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("nonexistent_binary_peppy_test_xyz"),
        "error should include the command that failed, got: {}",
        error_msg
    );

    assert!(
        build_result.log_path.exists(),
        "log file should exist at {:?}",
        build_result.log_path
    );

    let log_content =
        std::fs::read_to_string(&build_result.log_path).expect("should be able to read log file");
    assert!(
        !log_content.is_empty(),
        "log file should not be empty when a spawn failure occurs"
    );
    assert!(
        log_content.contains("[error]"),
        "log file should contain an [error] entry, got:\n{}",
        log_content
    );
    assert!(
        log_content.contains("build_cmd failed"),
        "log file should contain the failure message, got:\n{}",
        log_content
    );
    assert!(
        log_content.contains("nonexistent_binary_peppy_test_xyz"),
        "log file should contain the command that failed, got:\n{}",
        log_content
    );
}
