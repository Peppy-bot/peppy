mod common;

use common::{
    AbortOnDrop, FeedbackDrainOutcome, GoalEnd, NodeAddSource, StartedCoreNode, TEST_GIT_HASH,
    add_and_build_forking_node, assert_failure, build_staged_node, commit_path,
    create_tar_zst_from_dir, entity_artifact_path, is_process_running, send_node_add_and_wait,
    send_node_add_and_wait_with_force, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, wait_until_action_completes, wait_until_process_exits,
    wait_until_service_reachable, write_peppy_json5,
};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::failure_kind::PeppyFailureKind;
use config_test_support as test_helpers;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeRemoveRequest,
};
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use git2::{Repository, Signature};
use gix_url::Url as GitUrl;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedSender;
use {
    httptest::Expectation, httptest::Server, httptest::matchers::request,
    httptest::responders::status_code,
};

const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The `node_add` goal of the local directory `source_dir`. The directory
/// records the test git hash, as `peppy node sync` records it, so the add
/// verifies the hash and runs no sync of its own.
fn fs_add_goal(source_dir: &Path) -> NodeAddGoal {
    let peppy_dir = source_dir.join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git.hash");
    NodeAddGoal::new(source_dir, TEST_GIT_HASH, RESULT_TIMEOUT.as_secs())
}

/// An add that [`send_logged_node_add`] sent: the log path that the goal
/// response of the accepted goal names, and how the add ended.
struct LoggedAdd {
    log_path: PathBuf,
    end: GoalEnd<NodeAddResult>,
}

/// Sends `goal`, forwards each feedback line of the add to `feedback_tx`,
/// and returns the log path of the accepted goal with how the add ended.
/// Panics when the daemon refuses the goal.
async fn send_logged_node_add(
    started: &StartedCoreNode,
    goal: &NodeAddGoal,
    feedback_tx: &UnboundedSender<NodeAddFeedback>,
) -> LoggedAdd {
    let mut action_handle = common::send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        GOAL_TIMEOUT,
    )
    .await
    .expect("the node_add goal should be sent");
    let goal_response =
        common::goal_answer(&action_handle, NodeAddGoalResponse::decode, "goal response")
            .expect("the goal answer should decode")
            .unwrap_or_else(|refusal| panic!("the daemon refused the node_add goal: {refusal}"));
    let deadline = tokio::time::Instant::now() + RESULT_TIMEOUT;
    let drained = common::drain_feedback(
        &mut action_handle,
        |payload| NodeAddFeedback::decode(payload).expect("failed to decode NodeAddFeedback"),
        |feedback| {
            let _ = feedback_tx.send(feedback);
            ControlFlow::Continue(())
        },
        deadline,
    )
    .await;
    assert!(
        !matches!(drained, FeedbackDrainOutcome::TimedOut),
        "the add did not end within {RESULT_TIMEOUT:?}"
    );
    let end = common::fetch_end(
        &started.caller_handle,
        &action_handle,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        NodeAddResult::decode,
        "result",
    )
    .await
    .expect("the end of the add should be fetched");
    LoggedAdd {
        log_path: goal_response.log_path,
        end,
    }
}

/// Returns the number of tracked instances for the entity at `(name, tag)`.
fn entity_instance_count(node_stack: &node_stack::NodeStack, name: &str, tag: &str) -> usize {
    node_stack
        .find(name, tag)
        .expect("entity should exist")
        .read()
        .instances()
        .len()
}

/// Builds a minimal `peppy.json5` document string for a node with the given
/// name, tag, and optional `(name, tag)` dependencies.
fn minimal_node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> String {
    let depends_on = if deps.is_empty() {
        String::new()
    } else {
        let nodes = deps
            .iter()
            .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", link_id: "{n}" }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        format!(r#"depends_on: {{ nodes: [{nodes}] }},"#)
    };
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
                {depends_on}
            }},
            interfaces: {{}},
            execution: {{
                language: "rust",
                run_cmd: ["sleep", "10"]
            }}
        }}"#
    )
}

/// Builds a `peppy.json5` document with a custom `execution` body. Use when
/// the shape of `minimal_node_config` doesn't fit (e.g. a different
/// `run_cmd`, an added `build_cmd`, or no `run_cmd` at all). The interfaces
/// block is always `{}`.
fn node_config_with_execution(name: &str, tag: &str, execution_body: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{name}",
                tag: "{tag}",
            }},
            interfaces: {{}},
            execution: {execution_body}
        }}"#
    )
}

/// Creates a minimal node bundle (a [`minimal_node_config`] in a tar.zst)
/// suitable for HTTP source tests, and returns the compressed bundle bytes.
fn create_minimal_http_bundle(node_name: &str, node_tag: &str) -> Vec<u8> {
    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    // The archive goes next to the source dir, not inside it: the archive
    // file exists while the dir is walked, so inside it would be archived too.
    let source_dir = bundle_dir.path().join("source");
    std::fs::create_dir(&source_dir).expect("failed to create bundle source dir");
    std::fs::write(
        source_dir.join(NODE_CONFIG_FILE),
        minimal_node_config(node_name, node_tag, &[]),
    )
    .expect("failed to write manifest");
    let bundle_path = bundle_dir.path().join("node.tar.zst");
    create_tar_zst_from_dir(&source_dir, &bundle_path, ".");
    std::fs::read(&bundle_path).expect("failed to read bundle")
}

/// Starts an HTTP mock server serving `bundle_bytes` at `/bundles/node.tar.zst`.
/// Returns the server (must be kept alive) and the parsed URL.
fn serve_bundle_over_http(bundle_bytes: Vec<u8>) -> (Server, url::Url) {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/node.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/node.tar.zst").to_string())
        .expect("http bundle url should parse");
    (server, url)
}

/// Lists all file entries in tar.zst archive bytes (normalized paths without leading "./").
fn list_archive_entries(archive_bytes: &[u8]) -> Vec<String> {
    let decoder =
        zstd::stream::read::Decoder::new(archive_bytes).expect("failed to create decoder");
    let mut archive = tar::Archive::new(decoder);
    archive
        .entries()
        .expect("failed to read entries")
        .filter_map(|entry| {
            let entry = entry.expect("failed to read entry");
            if entry.header().entry_type().is_dir() {
                return None;
            }
            let path = entry.path().expect("failed to read path");
            let s = path.to_string_lossy().trim_start_matches("./").to_string();
            if s.is_empty() { None } else { Some(s) }
        })
        .collect()
}

#[test]
fn minimal_http_bundle_holds_only_the_node_config() {
    let bundle_bytes = create_minimal_http_bundle("bundle_node", "v1");

    assert_eq!(list_archive_entries(&bundle_bytes), [NODE_CONFIG_FILE]);
}

fn create_versioned_nodes_git_repo(to_path: impl AsRef<Path>) -> PathBuf {
    let base_path = to_path.as_ref();
    let repo_path = base_path.join("versioned_peppy_nodes_repo.git");
    std::fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");
    let signature =
        Signature::now("Peppy", "peppy@example.com").expect("failed to create signature");

    let uvc_dir = repo_path.join("nodes/uvc_camera");
    std::fs::create_dir_all(&uvc_dir).expect("failed to create uvc directories");

    let rel_config_path = Path::new("nodes/uvc_camera").join(NODE_CONFIG_FILE);

    std::fs::write(
        repo_path.join(&rel_config_path),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
            },
            interfaces: {
                topics: {
                    emits: [{ name: "/example" }]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    )
    .expect("failed to write uvc node v0.1.0");

    let commit_v1 = commit_path(
        &repo,
        &signature,
        &rel_config_path,
        "uvc_camera v0.1.0",
        &[],
    );
    repo.tag("v0.1.0", commit_v1.as_object(), &signature, "v0.1.0", false)
        .expect("failed to create v0.1.0 tag");

    std::fs::write(
        repo_path.join(&rel_config_path),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v2",
            },
            interfaces: {
                services: {
                    exposes: [
                        { name: "reset_sensor" }
                    ]
                }
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    )
    .expect("failed to write uvc node v0.2.0");

    let commit_v2 = commit_path(
        &repo,
        &signature,
        &rel_config_path,
        "uvc_camera v0.2.0",
        &[&commit_v1],
    );
    repo.tag("v0.2.0", commit_v2.as_object(), &signature, "v0.2.0", false)
        .expect("failed to create v0.2.0 tag");

    repo_path
}

/// Creates a git repository containing a single invalid `peppy.json5`
/// (missing required `execution` field).
fn create_git_repo_with_invalid_config(base_path: &Path) -> PathBuf {
    let repo_path = base_path.join("invalid_config_repo.git");
    std::fs::create_dir_all(&repo_path).expect("create repo dir");

    let repo = Repository::init(&repo_path).expect("init repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let config_rel = Path::new("nodes/bad_node/peppy.json5");
    std::fs::create_dir_all(repo_path.join("nodes/bad_node")).expect("create node dir");
    // Invalid config: has manifest but no execution.
    std::fs::write(
        repo_path.join(config_rel),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "bad_node",
                tag: "v1",
            },
            interfaces: {},
        }"#,
    )
    .expect("write invalid config");

    commit_path(&repo, &signature, config_rel, "initial commit", &[]);

    repo_path
}

#[path = "listen_for_node_add/basic_sources.rs"]
mod basic_sources;

#[path = "listen_for_node_add/repo_sources.rs"]
mod repo_sources;

#[path = "listen_for_node_add/failures.rs"]
mod failures;

#[path = "listen_for_node_add/replacement_and_lifecycle.rs"]
mod replacement_and_lifecycle;

#[path = "listen_for_node_add/concurrency_and_force.rs"]
mod concurrency_and_force;

#[path = "listen_for_node_add/implements.rs"]
mod implements;
