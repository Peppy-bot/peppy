mod common;

use common::{
    CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::QoSProfile;
use core_node::names;
use core_node::nodes_repo_cache_path;
use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
    RepoSourceKind,
};
use git2::{Repository, Signature};
use peppylib::ActionMessenger;
use peppylib::messaging::ResultStatus;
use std::path::Path;
use std::time::Duration;

/// Minimal valid peppy.json5 content for a node with the given name and tag.
fn minimal_peppy_json5(name: &str, tag: &str) -> String {
    format!(
        r#"{{
  peppy_schema: "node/v1",
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

/// Write an excluded_repositories.json5 file in the conf_dir.
fn write_excluded_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("excluded_repositories.json5"), content)
        .expect("write excluded repos file");
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

/// Send a refresh goal and wait for the result. The server publishes feedback
/// with wildcards at caller positions, so a concrete caller identity still
/// receives feedback over a real messenger; the mock adapter just doesn't
/// deliver feedback.
async fn send_refresh_and_wait(started: &StartedCoreNode) -> RefreshTestResult {
    send_refresh_inner(started, &started.core_node_name, CALLER_INSTANCE_ID).await
}

async fn send_refresh_and_wait_with_feedback(started: &StartedCoreNode) -> RefreshTestResult {
    send_refresh_and_wait(started).await
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
        common::core_node_target(&started.core_node_name),
        names::REPO_REFRESH_ACTION,
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

    // Drain feedback until the server closes the stream on completion, then
    // fetch the buffered result once.
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
            Err(_) => {}
        }
    }

    let fetch_timeout = deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(1));
    match ActionMessenger::request_result(&started.caller_handle, &action_handle, fetch_timeout)
        .await
    {
        Ok(reply) => match reply.status {
            ResultStatus::Completed | ResultStatus::Cancelled => {
                let result = RepoRefreshResult::decode(reply.body.as_ref())
                    .expect("decode repo_refresh result");
                RefreshTestResult {
                    goal_response,
                    feedbacks,
                    result,
                }
            }
            other => panic!("repo_refresh did not complete with a result: {other:?}"),
        },
        Err(err) => panic!("Failed to get result: {}", err),
    }
}

// ── Tests with real messenger (feedback assertions) ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_fs_discovers_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("test_repo");
    create_node_dir(&repo_dir, "my_sensor", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.goal_response.accepted, "goal should be accepted");
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "should find exactly 1 node"
    );

    let discovered: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();
    assert_eq!(discovered.len(), 1, "should receive 1 discovered feedback");
    let RepoRefreshFeedback::Discovered {
        item_name,
        item_tag,
        source_type,
        ..
    } = discovered[0]
    else {
        unreachable!()
    };
    assert_eq!(item_name, "my_sensor");
    assert_eq!(item_tag, "v1");
    assert_eq!(*source_type, RepoSourceKind::Fs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_multiple_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("multi_repo");
    create_node_dir(&repo_dir, "node_a", "v1");
    create_node_dir(&repo_dir, "node_b", "v2");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 2,
        "should find exactly 2 nodes"
    );

    let names: Vec<&str> = result
        .feedbacks
        .iter()
        .filter_map(|f| match f {
            RepoRefreshFeedback::Discovered { item_name, .. } => Some(item_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 2, "should receive 2 discovered feedbacks");
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
    create_node_dir(&repo_dir_a, "dup_node", "v1");
    create_node_dir(&repo_dir_b, "dup_node", "v1");

    // repo_a listed first (lower id), should take precedence
    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_dir_a.display(),
            repo_dir_b.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "dup_node:v1 should appear exactly once"
    );

    let discovered: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();
    assert_eq!(
        discovered.len(),
        1,
        "should receive exactly 1 feedback for deduplicated node"
    );
    let RepoRefreshFeedback::Discovered {
        item_name,
        item_tag,
        path,
        ..
    } = discovered[0]
    else {
        unreachable!()
    };
    assert_eq!(item_name, "dup_node");
    assert_eq!(item_tag, "v1");
    assert!(
        path.contains("repo_a"),
        "first listed repo should take precedence, path was: {}",
        path
    );
}

// ── Tests with mock messenger (no feedback needed) ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_url_skipped() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo_dir, "real_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "url", "url": "https://example.com/packages" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
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

    // FS repo with a single node
    let repo_dir = started.peppy_dirs.root().join("cached_repo");
    create_node_dir(&repo_dir, "cached_node", "v2");

    // Local git repo with a node in a subfolder
    let git_repo_path = started.peppy_dirs.root().join("git_test_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let node_subdir = git_repo_path.join("nodes/git_node");
    std::fs::create_dir_all(&node_subdir).expect("create git node dir");
    std::fs::write(
        node_subdir.join(NODE_CONFIG_FILE),
        minimal_peppy_json5("git_node", "v1"),
    )
    .expect("write git node peppy.json5");

    let rel_config_path = Path::new("nodes/git_node/peppy.json5");
    let mut index = repo.index().expect("open index");
    index
        .add_path(rel_config_path)
        .expect("add config to index");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .expect("commit");

    let git_repo_url = format!("file://{}", git_repo_path.display());
    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "git", "url": "{}" }}]"#,
            repo_dir.display(),
            git_repo_url,
        ),
    );

    let result = send_refresh_and_wait(&started).await;
    assert!(result.result.success, "refresh should succeed");

    let cache_path = nodes_repo_cache_path(&started.peppy_dirs);
    assert!(cache_path.exists(), "cache file should exist");

    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse cache JSON5");
    assert_eq!(entries.len(), 2, "cache should contain 2 nodes (FS + git)");

    let fs_entry = entries
        .iter()
        .find(|e| e["source_type"] == "fs")
        .expect("should have an fs entry");
    let git_entry = entries
        .iter()
        .find(|e| e["source_type"] == "git")
        .expect("should have a git entry");

    assert_eq!(fs_entry["node_name"], "cached_node");
    assert_eq!(fs_entry["node_tag"], "v2");
    assert!(
        fs_entry.get("resolved_ref").is_none(),
        "fs entries should not carry a resolved_ref in the cache"
    );

    assert_eq!(git_entry["node_name"], "git_node");
    assert_eq!(git_entry["node_tag"], "v1");
    assert_eq!(git_entry["path"], "nodes/git_node/peppy.json5");
    assert_eq!(git_entry["source_uri"], git_repo_url);
    let resolved_ref = git_entry
        .get("resolved_ref")
        .and_then(|v| v.as_str())
        .expect("git entry should carry resolved_ref");
    assert!(
        !resolved_ref.is_empty(),
        "resolved_ref should be a non-empty branch name"
    );
}

/// When two repositories provide the same node, the cache should contain
/// both entries. Both carry a `sha256` content fingerprint, and lookup
/// picks the entry from the highest-priority (lowest-id) repository.
/// `total_nodes_found` reflects the unique `(name, tag)` count; feedback
/// is emitted once per unique node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_cache_includes_duplicates() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir_a = started.peppy_dirs.root().join("dup_cache_a");
    let repo_dir_b = started.peppy_dirs.root().join("dup_cache_b");
    create_node_dir(&repo_dir_a, "shared_node", "v1");
    create_node_dir(&repo_dir_b, "shared_node", "v1");
    // unique node only in repo_b
    create_node_dir(&repo_dir_b, "unique_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_dir_a.display(),
            repo_dir_b.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 2,
        "unique node count should be 2 (shared_node + unique_node)"
    );

    // Feedback is emitted once per unique (name, tag); the second
    // repo's shared_node is silently cached but not re-announced.
    let feedback_names: Vec<&str> = result
        .feedbacks
        .iter()
        .filter_map(|f| match f {
            RepoRefreshFeedback::Discovered { item_name, .. } => Some(item_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        feedback_names.len(),
        2,
        "should receive 2 discovered feedbacks (one per unique node)"
    );
    assert!(feedback_names.contains(&"shared_node"));
    assert!(feedback_names.contains(&"unique_node"));

    // Cache keeps both `shared_node` entries (no `duplicate` flag; the
    // `sha256` field tells them apart for users who need to pick one).
    let cache_path = nodes_repo_cache_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json5::from_str(&content).expect("parse cache");
    assert_eq!(
        entries.len(),
        3,
        "cache should contain 3 entries (both shared_node copies + unique_node)"
    );

    let shared_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["node_name"] == "shared_node")
        .collect();
    assert_eq!(
        shared_entries.len(),
        2,
        "shared_node should appear twice in cache"
    );
    assert!(
        shared_entries
            .iter()
            .any(|e| e["path"].as_str().unwrap().contains("dup_cache_a")),
        "one entry should be from repo_a"
    );
    assert!(
        shared_entries
            .iter()
            .any(|e| e["path"].as_str().unwrap().contains("dup_cache_b")),
        "other entry should be from repo_b"
    );
    for entry in &shared_entries {
        assert!(
            entry.get("duplicate").is_none(),
            "no entry should carry the legacy `duplicate` flag"
        );
        assert!(
            entry
                .get("sha256")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "each entry should carry a non-empty sha256 fingerprint"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_empty_repos() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, "[]");

    let result = send_refresh_and_wait(&started).await;

    assert!(result.goal_response.accepted, "goal should be accepted");
    assert!(
        result.result.success,
        "refresh should succeed with empty repos"
    );
    assert_eq!(
        result.result.total_nodes_found, 0,
        "no nodes should be found with empty repos"
    );
}

// ── Exclusion tests ──────────────────────────────────────────────

/// When one of two FS repos is excluded, only nodes from the non-excluded
/// repo should appear in feedback and the excluded repo should be reported
/// as excluded feedback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excludes_fs_repo_with_feedback() {
    let started = start_core_node_with_real_messenger().await;

    let repo_a = started.peppy_dirs.root().join("repo_a");
    let repo_b = started.peppy_dirs.root().join("repo_b");
    create_node_dir(&repo_a, "node_a", "v1");
    create_node_dir(&repo_b, "node_b", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_a.display(),
            repo_b.display()
        ),
    );
    write_excluded_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_b.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "only node_a should be counted"
    );

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Excluded { .. }))
        .collect();
    let discovered_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();

    assert_eq!(
        excluded_feedbacks.len(),
        1,
        "should receive 1 excluded feedback"
    );
    let RepoRefreshFeedback::Excluded {
        source_type,
        identity,
    } = excluded_feedbacks[0]
    else {
        unreachable!()
    };
    assert_eq!(*source_type, RepoSourceKind::Fs);
    assert!(
        identity.contains("repo_b"),
        "excluded feedback identity should reference repo_b, got: {}",
        identity
    );

    assert_eq!(
        discovered_feedbacks.len(),
        1,
        "should receive 1 discovered feedback"
    );
    let RepoRefreshFeedback::Discovered { item_name, .. } = discovered_feedbacks[0] else {
        unreachable!()
    };
    assert_eq!(item_name, "node_a");
}

/// Excluding a subdirectory within an FS repo should prune that subtree
/// without excluding the entire repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excludes_fs_subdirectory_with_feedback() {
    let started = start_core_node_with_real_messenger().await;

    let repo = started.peppy_dirs.root().join("mixed_repo");
    create_node_dir(&repo, "keep_node", "v1");
    create_node_dir(&repo, "secret_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo.display()
        ),
    );
    write_excluded_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo.join("secret_node_v1").display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "only keep_node should be found"
    );

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    let RepoRefreshFeedback::Discovered { item_name, .. } = discovered_feedbacks[0] else {
        unreachable!()
    };
    assert_eq!(item_name, "keep_node");

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Excluded { .. }))
        .collect();
    assert_eq!(
        excluded_feedbacks.len(),
        1,
        "should receive 1 excluded feedback for subdirectory exclusion"
    );
    let RepoRefreshFeedback::Excluded {
        source_type,
        identity,
    } = excluded_feedbacks[0]
    else {
        unreachable!()
    };
    assert_eq!(*source_type, RepoSourceKind::Fs);
    assert!(
        identity.contains("secret_node"),
        "excluded feedback identity should reference secret_node, got: {}",
        identity
    );
}

/// When both a repo-level exclusion and a subdirectory exclusion are present,
/// feedback should be reported for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_reports_both_repo_and_subdirectory_exclusions() {
    let started = start_core_node_with_real_messenger().await;

    let repo_a = started.peppy_dirs.root().join("repo_a");
    let repo_b = started.peppy_dirs.root().join("repo_b");
    create_node_dir(&repo_a, "keep_node", "v1");
    create_node_dir(&repo_a, "secret_node", "v1");
    create_node_dir(&repo_b, "other_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_a.display(),
            repo_b.display()
        ),
    );
    write_excluded_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_b.display(),
            repo_a.join("secret_node_v1").display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "only keep_node should be counted"
    );

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Excluded { .. }))
        .collect();
    assert_eq!(
        excluded_feedbacks.len(),
        2,
        "should receive 2 excluded feedbacks (repo-level + subdirectory)"
    );

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    let RepoRefreshFeedback::Discovered { item_name, .. } = discovered_feedbacks[0] else {
        unreachable!()
    };
    assert_eq!(item_name, "keep_node");
}

/// Excluded repos should not appear in the nodes.json5 cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excluded_repos_not_in_cache() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("cache_repo_a");
    let repo_b = started.peppy_dirs.root().join("cache_repo_b");
    create_node_dir(&repo_a, "cached_node", "v1");
    create_node_dir(&repo_b, "excluded_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "fs", "path": "{}" }}]"#,
            repo_a.display(),
            repo_b.display()
        ),
    );
    write_excluded_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_b.display()
        ),
    );

    let result = send_refresh_and_wait(&started).await;
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_nodes_found, 1);

    let cache_path = nodes_repo_cache_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json5::from_str(&content).expect("parse cache");
    assert_eq!(
        entries.len(),
        1,
        "cache should only contain non-excluded nodes"
    );
    assert_eq!(entries[0]["node_name"], "cached_node");
}

/// When an excluded git repo is listed, it should be skipped entirely
/// (no clone attempt) and reported as excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excludes_git_repo() {
    let started = start_core_node_with_real_messenger().await;

    let repo = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo, "fs_node", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "git", "url": "https://example.com/nonexistent.git" }}]"#,
            repo.display()
        ),
    );
    write_excluded_repositories_json5(
        &started,
        r#"[{ "id": 1, "type": "git", "url": "https://example.com/nonexistent.git" }]"#,
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_nodes_found, 1);

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Excluded { .. }))
        .collect();
    assert_eq!(excluded_feedbacks.len(), 1);
    let RepoRefreshFeedback::Excluded { source_type, .. } = excluded_feedbacks[0] else {
        unreachable!()
    };
    assert_eq!(*source_type, RepoSourceKind::Git);

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> = result
        .feedbacks
        .iter()
        .filter(|f| matches!(f, RepoRefreshFeedback::Discovered { .. }))
        .collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    let RepoRefreshFeedback::Discovered { item_name, .. } = discovered_feedbacks[0] else {
        unreachable!()
    };
    assert_eq!(item_name, "fs_node");
}

/// End-to-end coverage of interface discovery: refresh writes
/// `interfaces.json5` with the expected shape (interface_name + tag +
/// sha256), the result reports the interface count, and feedback
/// includes the discovered interface tagged with kind = Interface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_discovers_interfaces() {
    use core_node::interfaces_repo_cache_path;
    use core_node_api::encoding::RepoItemKind;

    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("iface_repo");
    let iface_dir = repo_dir.join("uvc_camera");
    std::fs::create_dir_all(&iface_dir).expect("create iface dir");
    let manifest_body = r#"{
  peppy_schema: "interface/v1",
  manifest: { name: "uvc_camera", tag: "v1", labels: ["uvc", "camera"] },
  interfaces: {}
}"#;
    std::fs::write(iface_dir.join("peppy.json5"), manifest_body).expect("write interface manifest");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_interfaces_found, 1);

    let Some(RepoRefreshFeedback::Discovered {
        item_name,
        item_tag,
        sha256,
        ..
    }) = result.feedbacks.iter().find(|f| {
        matches!(
            f,
            RepoRefreshFeedback::Discovered {
                kind: RepoItemKind::Interface,
                ..
            }
        )
    })
    else {
        panic!("interface discovery feedback")
    };
    assert_eq!(item_name, "uvc_camera");
    assert_eq!(item_tag, "v1");
    assert!(
        !sha256.is_empty(),
        "feedback should carry the sha256 fingerprint"
    );

    let cache_path = interfaces_repo_cache_path(&started.peppy_dirs);
    assert!(cache_path.exists(), "interfaces cache should be written");
    let content = std::fs::read_to_string(&cache_path).expect("read interfaces cache");
    let entries: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse interfaces cache");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["interface_name"], "uvc_camera");
    assert_eq!(entries[0]["tag"], "v1");
    assert_eq!(entries[0]["source_type"], "fs");
    assert!(
        entries[0]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("uvc_camera/peppy.json5")),
        "path should point at the manifest file: {:?}",
        entries[0]["path"]
    );
    assert!(
        entries[0]["sha256"].as_str().is_some_and(|s| !s.is_empty()),
        "sha256 should be non-empty"
    );
}

/// End-to-end coverage of node discovery: refresh writes `nodes.json5`
/// with the expected shape (node_name + node_tag + sha256), the result
/// reports the node count, and feedback includes the discovered node
/// tagged with kind = Node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_discovers_nodes() {
    use core_node_api::encoding::RepoItemKind;

    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("node_repo");
    create_node_dir(&repo_dir, "my_sensor", "v1");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_nodes_found, 1);

    let Some(RepoRefreshFeedback::Discovered {
        item_name,
        item_tag,
        sha256,
        ..
    }) = result.feedbacks.iter().find(|f| {
        matches!(
            f,
            RepoRefreshFeedback::Discovered {
                kind: RepoItemKind::Node,
                ..
            }
        )
    })
    else {
        panic!("node discovery feedback")
    };
    assert_eq!(item_name, "my_sensor");
    assert_eq!(item_tag, "v1");
    assert!(
        !sha256.is_empty(),
        "feedback should carry the sha256 fingerprint"
    );

    let cache_path = nodes_repo_cache_path(&started.peppy_dirs);
    assert!(cache_path.exists(), "nodes cache should be written");
    let content = std::fs::read_to_string(&cache_path).expect("read nodes cache");
    let entries: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse nodes cache");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["node_name"], "my_sensor");
    assert_eq!(entries[0]["node_tag"], "v1");
    assert_eq!(entries[0]["source_type"], "fs");
    assert!(
        entries[0]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("peppy.json5")),
        "path should point at the manifest file: {:?}",
        entries[0]["path"]
    );
    assert!(
        entries[0]["sha256"].as_str().is_some_and(|s| !s.is_empty()),
        "sha256 should be non-empty"
    );
}

/// End-to-end coverage of launcher discovery: refresh writes
/// `launchers.json5` with the expected shape (launcher_name + sha256),
/// the result reports the launcher count, and feedback includes the
/// discovered launcher tagged with kind = Launcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_discovers_launchers() {
    use core_node::launchers_repo_cache_path;
    use core_node_api::encoding::RepoItemKind;

    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("launcher_repo");
    std::fs::create_dir_all(&repo_dir).expect("create launcher repo dir");
    let manifest_body = r#"{
  peppy_schema: "launcher/v1",
  deployments: []
}"#;
    std::fs::write(repo_dir.join("openarm01_teleop.json5"), manifest_body)
        .expect("write launcher manifest");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_launchers_found, 1);

    let Some(RepoRefreshFeedback::Discovered {
        item_name,
        item_tag,
        sha256,
        ..
    }) = result.feedbacks.iter().find(|f| {
        matches!(
            f,
            RepoRefreshFeedback::Discovered {
                kind: RepoItemKind::Launcher,
                ..
            }
        )
    })
    else {
        panic!("launcher discovery feedback")
    };
    assert_eq!(item_name, "openarm01_teleop");
    assert!(
        item_tag.is_empty(),
        "launcher feedback should not carry a tag"
    );
    assert!(
        !sha256.is_empty(),
        "feedback should carry the sha256 fingerprint"
    );

    let cache_path = launchers_repo_cache_path(&started.peppy_dirs);
    assert!(cache_path.exists(), "launchers cache should be written");
    let content = std::fs::read_to_string(&cache_path).expect("read launchers cache");
    let entries: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse launchers cache");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["launcher_name"], "openarm01_teleop");
    assert_eq!(entries[0]["source_type"], "fs");
    assert!(
        entries[0]["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("openarm01_teleop.json5")),
        "path should point at the .json5 file: {:?}",
        entries[0]["path"]
    );
    assert!(
        entries[0]["sha256"].as_str().is_some_and(|s| !s.is_empty()),
        "sha256 should be non-empty"
    );
}
