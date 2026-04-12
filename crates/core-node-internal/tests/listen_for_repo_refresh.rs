mod common;

use common::{
    CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::QoSProfile;
use core_node::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
};
use core_node::names;
use peppylib::ActionMessenger;
use std::time::Duration;

/// Minimal valid peppy.json5 content for a node with the given name and tag.
fn minimal_peppy_json5(name: &str, tag: &str) -> String {
    format!(
        r#"{{
  schema_version: 1,
  manifest: {{
    name: "{name}",
    tag: "{tag}",
  }},
  interfaces: {{}},
  execution: {{
    language: "rust",
    build_cmd: ["true"],
    run_cmd: ["true"],
  }},
}}"#
    )
}

/// Write a repositories.json5 file in the conf_dir of the started core node.
fn write_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("repositories.json5"), content).expect("write repos file");
}

/// Create a directory with a valid peppy.json5 inside it.
fn create_node_dir(base: &std::path::Path, name: &str, tag: &str) -> std::path::PathBuf {
    let dir = base.join(format!("{name}_{tag}"));
    std::fs::create_dir_all(&dir).expect("create node dir");
    std::fs::write(dir.join(NODE_CONFIG_FILE), minimal_peppy_json5(name, tag))
        .expect("write peppy.json5");
    dir
}

struct RefreshTestResult {
    goal_response: RepoRefreshGoalResponse,
    feedbacks: Vec<RepoRefreshFeedback>,
    result: RepoRefreshResult,
}

/// Send a refresh goal and wait for the result. Uses mock-compatible identifiers
/// (no feedback will be received with mock adapter).
async fn send_refresh_and_wait(started: &StartedCoreNode) -> RefreshTestResult {
    send_refresh_inner(started, &started.core_node_name, CALLER_INSTANCE_ID).await
}

/// Send a refresh goal and wait for the result with wildcard identifiers so
/// feedback is received (requires real messenger).
async fn send_refresh_and_wait_with_feedback(started: &StartedCoreNode) -> RefreshTestResult {
    send_refresh_inner(started, "*", "*").await
}

async fn send_refresh_inner(
    started: &StartedCoreNode,
    caller_core_node: &str,
    caller_instance_id: &str,
) -> RefreshTestResult {
    let goal = RepoRefreshGoal;
    let goal_payload = goal.encode().expect("encode goal");

    let mut action_handle = ActionMessenger::send_goal(
        &started.caller_handle,
        caller_core_node,
        caller_instance_id,
        &started.core_node_name,
        names::REPO_REFRESH_ACTION,
        Some(&started.core_node_name),
        None,
        goal_payload,
        QoSProfile::default(),
        Duration::from_secs(5),
    )
    .await
    .expect("send goal should succeed");

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response =
        RepoRefreshGoalResponse::decode(&goal_response_payload).expect("decode goal response");

    let mut feedbacks = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    loop {
        // Drain feedback
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("Timeout waiting for repo_refresh result");
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = RepoRefreshFeedback::decode(payload.as_ref()) {
                        feedbacks.push(feedback);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        if tokio::time::Instant::now() >= deadline {
            panic!("Timeout waiting for repo_refresh result");
        }

        let poll_timeout = Duration::from_millis(200);
        match ActionMessenger::request_result(&started.caller_handle, &action_handle, poll_timeout)
            .await
        {
            Ok(msg) => {
                let payload = msg.payload();
                match RepoRefreshResult::decode(&payload) {
                    Ok(result) => {
                        // Drain remaining feedback
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = RepoRefreshFeedback::decode(payload.as_ref()) {
                                feedbacks.push(feedback);
                            }
                        }
                        return RefreshTestResult {
                            goal_response,
                            feedbacks,
                            result,
                        };
                    }
                    Err(_) => {
                        if peppylib::encoding::is_result_pending(payload.as_ref()) {
                            // Result not ready yet, keep polling
                        } else {
                            panic!("Unexpected decode error for result");
                        }
                    }
                }
            }
            Err(peppylib::PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => panic!("Failed to get result: {}", err),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── Tests with real messenger (feedback assertions) ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_fs_discovers_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("test_repo");
    create_node_dir(&repo_dir, "my_sensor", "1.0.0");

    write_repositories_json5(
        &started,
        &format!(r#"[{{ "type": "fs", "path": "{}" }}]"#, repo_dir.display()),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.goal_response.accepted, "goal should be accepted");
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "should find exactly 1 node"
    );

    assert_eq!(result.feedbacks.len(), 1, "should receive 1 feedback");
    assert_eq!(result.feedbacks[0].node_name, "my_sensor");
    assert_eq!(result.feedbacks[0].node_tag, "1.0.0");
    assert_eq!(result.feedbacks[0].source_type, "fs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_multiple_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("multi_repo");
    create_node_dir(&repo_dir, "node_a", "1.0.0");
    create_node_dir(&repo_dir, "node_b", "2.0.0");

    write_repositories_json5(
        &started,
        &format!(r#"[{{ "type": "fs", "path": "{}" }}]"#, repo_dir.display()),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 2,
        "should find exactly 2 nodes"
    );

    assert_eq!(result.feedbacks.len(), 2, "should receive 2 feedbacks");
    let names: Vec<&str> = result
        .feedbacks
        .iter()
        .map(|f| f.node_name.as_str())
        .collect();
    assert!(names.contains(&"node_a"), "should contain node_a");
    assert!(names.contains(&"node_b"), "should contain node_b");
}

/// When two repositories contain a node with the same `name:tag`, only the
/// entry from the repository listed first in `repositories.json5` should be
/// reported. This verifies that the ordering-based precedence rule is enforced
/// and that no duplicate feedback is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_deduplication() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir_a = started.peppy_dirs.root().join("repo_a");
    let repo_dir_b = started.peppy_dirs.root().join("repo_b");
    create_node_dir(&repo_dir_a, "dup_node", "0.1.0");
    create_node_dir(&repo_dir_b, "dup_node", "0.1.0");

    // repo_a listed first, should take precedence
    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "type": "fs", "path": "{}" }}, {{ "type": "fs", "path": "{}" }}]"#,
            repo_dir_a.display(),
            repo_dir_b.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "dup_node:0.1.0 should appear exactly once"
    );

    assert_eq!(
        result.feedbacks.len(),
        1,
        "should receive exactly 1 feedback for deduplicated node"
    );
    assert_eq!(result.feedbacks[0].node_name, "dup_node");
    assert_eq!(result.feedbacks[0].node_tag, "0.1.0");
    assert!(
        result.feedbacks[0].path.contains("repo_a"),
        "first listed repo should take precedence, path was: {}",
        result.feedbacks[0].path
    );
}

// ── Tests with mock messenger (no feedback needed) ───��──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_url_skipped() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo_dir, "real_node", "0.1.0");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "type": "url", "url": "https://example.com/packages" }}, {{ "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait(&started).await;

    assert!(
        result.result.success,
        "refresh should succeed even with URL repo"
    );
    assert_eq!(
        result.result.total_nodes_found, 1,
        "FS repo node should still be found"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_cache_written() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("cached_repo");
    create_node_dir(&repo_dir, "cached_node", "2.0.0");

    write_repositories_json5(
        &started,
        &format!(r#"[{{ "type": "fs", "path": "{}" }}]"#, repo_dir.display()),
    );

    let result = send_refresh_and_wait(&started).await;
    assert!(result.result.success, "refresh should succeed");

    let cache_path = started.peppy_dirs.cache_dir().join("repositories.json5");
    assert!(cache_path.exists(), "cache file should exist");

    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse cache JSON");
    assert!(
        entries.is_empty(),
        "cache should be empty for FS-only repos"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_empty_repos() {
    let started = start_core_node_with_mock_messenger().await;

    let result = send_refresh_and_wait(&started).await;

    assert!(result.goal_response.accepted, "goal should be accepted");
    assert!(
        result.result.success,
        "refresh should succeed with defaults"
    );
}
