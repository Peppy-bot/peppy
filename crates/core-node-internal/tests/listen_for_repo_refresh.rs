mod common;

use common::{
    CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::QoSProfile;
use core_node::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
    RepoSourceKind,
};
use core_node::names;
use git2::{Repository, Signature};
use peppylib::ActionMessenger;
use std::path::Path;
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

/// Minimal valid peppy.json5 content for a root node that declares variants
/// (no execution block — execution comes from the variant subdirectories).
fn root_with_variants_peppy_json5(name: &str, tag: &str, variants: &[(&str, &str)]) -> String {
    let variant_entries: Vec<String> = variants
        .iter()
        .map(|(vname, vpath)| {
            format!(r#"        {{ name: "{vname}", source: {{ local: "{vpath}" }} }}"#)
        })
        .collect();
    format!(
        r#"{{
  schema_version: 1,
  manifest: {{
    name: "{name}",
    tag: "{tag}",
    variants: [
{variants_list}
    ]
  }},
  interfaces: {{}},
}}"#,
        variants_list = variant_entries.join(",\n")
    )
}

/// Minimal valid variant peppy.json5 (no manifest, execution only).
fn variant_peppy_json5() -> &'static str {
    r#"{
  schema_version: 1,
  execution: {
    language: "rust",
    build_cmd: ["true"],
    run_cmd: ["true"],
  },
}"#
}

/// Create a node directory with variants. Returns the root node path.
fn create_node_dir_with_variants(
    base: &std::path::Path,
    name: &str,
    tag: &str,
    variant_names: &[&str],
) -> std::path::PathBuf {
    let dir = base.join(format!("{name}_{tag}"));
    std::fs::create_dir_all(&dir).expect("create node dir");

    let variants: Vec<(&str, String)> = variant_names
        .iter()
        .map(|vname| (*vname, format!("./variants/{vname}")))
        .collect();
    let variant_refs: Vec<(&str, &str)> = variants.iter().map(|(n, p)| (*n, p.as_str())).collect();

    std::fs::write(
        dir.join(NODE_CONFIG_FILE),
        root_with_variants_peppy_json5(name, tag, &variant_refs),
    )
    .expect("write root peppy.json5");

    for vname in variant_names {
        let variant_dir = dir.join("variants").join(vname);
        std::fs::create_dir_all(&variant_dir).expect("create variant dir");
        std::fs::write(variant_dir.join(NODE_CONFIG_FILE), variant_peppy_json5())
            .expect("write variant peppy.json5");
    }

    dir
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

    assert_eq!(result.feedbacks.len(), 1, "should receive 1 feedback");
    assert_eq!(result.feedbacks[0].node_name, "my_sensor");
    assert_eq!(result.feedbacks[0].node_tag, "1.0.0");
    assert_eq!(result.feedbacks[0].source_type, RepoSourceKind::Fs);
    assert!(
        result.feedbacks[0].variants.is_empty(),
        "node without variants should have empty variants list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_multiple_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("multi_repo");
    create_node_dir(&repo_dir, "node_a", "1.0.0");
    create_node_dir(&repo_dir, "node_b", "2.0.0");

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

/// A root node that declares variants should be discovered as a single node
/// with the variant names attached. The variant subdirectories (which have
/// their own peppy.json5 without a manifest) must NOT be counted as separate
/// nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_node_with_variants_counted_once() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("variant_repo");
    create_node_dir_with_variants(&repo_dir, "my_camera", "0.1.0", &["default", "mock", "gpu"]);

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
        "node with variants should be counted as 1 node, not 1 + variant count"
    );

    assert_eq!(
        result.feedbacks.len(),
        1,
        "should receive exactly 1 feedback"
    );
    assert_eq!(result.feedbacks[0].node_name, "my_camera");
    assert_eq!(result.feedbacks[0].node_tag, "0.1.0");
    assert_eq!(
        result.feedbacks[0].variants,
        vec!["default", "mock", "gpu"],
        "feedback should include declared variant names"
    );
}

/// When a repo has both a plain node and a node with variants, the total count
/// should reflect only root nodes (not variants).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_mixed_plain_and_variant_nodes() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("mixed_repo");
    create_node_dir(&repo_dir, "plain_node", "1.0.0");
    create_node_dir_with_variants(&repo_dir, "variant_node", "1.0.0", &["default", "sim"]);

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
        "should count 2 root nodes (plain + variant node)"
    );

    assert_eq!(result.feedbacks.len(), 2, "should receive 2 feedbacks");

    let plain = result
        .feedbacks
        .iter()
        .find(|f| f.node_name == "plain_node")
        .expect("should have plain_node feedback");
    assert!(
        plain.variants.is_empty(),
        "plain node should have no variants"
    );

    let variant = result
        .feedbacks
        .iter()
        .find(|f| f.node_name == "variant_node")
        .expect("should have variant_node feedback");
    assert_eq!(
        variant.variants,
        vec!["default", "sim"],
        "variant node should carry its variant names"
    );
}

// ── Tests with mock messenger (no feedback needed) ─────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_url_skipped() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo_dir, "real_node", "0.1.0");

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
    create_node_dir(&repo_dir, "cached_node", "2.0.0");

    // Local git repo with a node in a subfolder
    let git_repo_path = started.peppy_dirs.root().join("git_test_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let node_subdir = git_repo_path.join("nodes/git_node");
    std::fs::create_dir_all(&node_subdir).expect("create git node dir");
    std::fs::write(
        node_subdir.join(NODE_CONFIG_FILE),
        minimal_peppy_json5("git_node", "1.0.0"),
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

    // Use file:// protocol so git2 shallow clone works with local repos
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

    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    assert!(cache_path.exists(), "cache file should exist");

    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse cache JSON");
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
    assert_eq!(fs_entry["node_tag"], "2.0.0");

    assert_eq!(git_entry["node_name"], "git_node");
    assert_eq!(git_entry["node_tag"], "1.0.0");
    assert_eq!(git_entry["path"], "nodes/git_node");
    assert_eq!(git_entry["source_uri"], git_repo_url);
}

/// Verify that the packages.json5 cache includes variant names for nodes that
/// declare them, and omits the field for plain nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_cache_includes_variants() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("cache_variant_repo");
    create_node_dir(&repo_dir, "plain", "1.0.0");
    create_node_dir_with_variants(&repo_dir, "with_variants", "1.0.0", &["default", "gpu"]);

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    );

    let result = send_refresh_and_wait(&started).await;
    assert!(result.result.success, "refresh should succeed");
    assert_eq!(result.result.total_nodes_found, 2);

    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse cache");

    let plain_entry = entries
        .iter()
        .find(|e| e["node_name"] == "plain")
        .expect("should have plain entry");
    assert!(
        plain_entry.get("variants").is_none(),
        "plain node should not have variants key in cache"
    );

    let variant_entry = entries
        .iter()
        .find(|e| e["node_name"] == "with_variants")
        .expect("should have with_variants entry");
    let cached_variants: Vec<&str> = variant_entry["variants"]
        .as_array()
        .expect("variants should be an array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        cached_variants,
        vec!["default", "gpu"],
        "cached variants should match declared variant names"
    );
}

/// When two repositories provide the same node, the cache should contain both
/// entries — the first as non-duplicate and the second marked as duplicate.
/// The total_nodes_found count should only reflect unique (non-duplicate) nodes
/// and feedback should only be emitted for non-duplicates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_cache_includes_duplicates() {
    let started = start_core_node_with_real_messenger().await;

    let repo_dir_a = started.peppy_dirs.root().join("dup_cache_a");
    let repo_dir_b = started.peppy_dirs.root().join("dup_cache_b");
    create_node_dir(&repo_dir_a, "shared_node", "1.0.0");
    create_node_dir(&repo_dir_b, "shared_node", "1.0.0");
    // unique node only in repo_b
    create_node_dir(&repo_dir_b, "unique_node", "1.0.0");

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

    // Feedback should only contain non-duplicate entries
    assert_eq!(
        result.feedbacks.len(),
        2,
        "should receive 2 feedbacks (one per unique node)"
    );
    let feedback_names: Vec<&str> = result
        .feedbacks
        .iter()
        .map(|f| f.node_name.as_str())
        .collect();
    assert!(feedback_names.contains(&"shared_node"));
    assert!(feedback_names.contains(&"unique_node"));

    // Cache should contain all 3 entries (including the duplicate)
    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse cache");
    assert_eq!(
        entries.len(),
        3,
        "cache should contain 3 entries (2 unique + 1 duplicate)"
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

    let primary = shared_entries
        .iter()
        .find(|e| e.get("duplicate").is_none())
        .expect("should have a non-duplicate shared_node");
    assert!(
        primary["path"].as_str().unwrap().contains("dup_cache_a"),
        "primary should be from repo_a (higher priority)"
    );

    let dup = shared_entries
        .iter()
        .find(|e| e.get("duplicate").and_then(|v| v.as_bool()) == Some(true))
        .expect("should have a duplicate shared_node");
    assert!(
        dup["path"].as_str().unwrap().contains("dup_cache_b"),
        "duplicate should be from repo_b"
    );
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
    create_node_dir(&repo_a, "node_a", "1.0.0");
    create_node_dir(&repo_b, "node_b", "1.0.0");

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

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| f.excluded).collect();
    let discovered_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| !f.excluded).collect();

    assert_eq!(
        excluded_feedbacks.len(),
        1,
        "should receive 1 excluded feedback"
    );
    assert_eq!(excluded_feedbacks[0].source_type, RepoSourceKind::Fs);
    assert!(
        excluded_feedbacks[0].path.contains("repo_b"),
        "excluded feedback path should reference repo_b, got: {}",
        excluded_feedbacks[0].path
    );

    assert_eq!(
        discovered_feedbacks.len(),
        1,
        "should receive 1 discovered feedback"
    );
    assert_eq!(discovered_feedbacks[0].node_name, "node_a");
}

/// Excluding a subdirectory within an FS repo should prune that subtree
/// without excluding the entire repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excludes_fs_subdirectory_with_feedback() {
    let started = start_core_node_with_real_messenger().await;

    let repo = started.peppy_dirs.root().join("mixed_repo");
    create_node_dir(&repo, "keep_node", "1.0.0");
    create_node_dir(&repo, "secret_node", "1.0.0");

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
            repo.join("secret_node_1.0.0").display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "only keep_node should be found"
    );

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| !f.excluded).collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    assert_eq!(discovered_feedbacks[0].node_name, "keep_node");

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| f.excluded).collect();
    assert_eq!(
        excluded_feedbacks.len(),
        1,
        "should receive 1 excluded feedback for subdirectory exclusion"
    );
    assert_eq!(excluded_feedbacks[0].source_type, RepoSourceKind::Fs);
    assert!(
        excluded_feedbacks[0].path.contains("secret_node"),
        "excluded feedback path should reference secret_node, got: {}",
        excluded_feedbacks[0].path
    );
}

/// When both a repo-level exclusion and a subdirectory exclusion are present,
/// feedback should be reported for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_reports_both_repo_and_subdirectory_exclusions() {
    let started = start_core_node_with_real_messenger().await;

    let repo_a = started.peppy_dirs.root().join("repo_a");
    let repo_b = started.peppy_dirs.root().join("repo_b");
    create_node_dir(&repo_a, "keep_node", "1.0.0");
    create_node_dir(&repo_a, "secret_node", "1.0.0");
    create_node_dir(&repo_b, "other_node", "1.0.0");

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
            repo_a.join("secret_node_1.0.0").display()
        ),
    );

    let result = send_refresh_and_wait_with_feedback(&started).await;

    assert!(result.result.success, "refresh should succeed");
    assert_eq!(
        result.result.total_nodes_found, 1,
        "only keep_node should be counted"
    );

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| f.excluded).collect();
    assert_eq!(
        excluded_feedbacks.len(),
        2,
        "should receive 2 excluded feedbacks (repo-level + subdirectory)"
    );

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| !f.excluded).collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    assert_eq!(discovered_feedbacks[0].node_name, "keep_node");
}

/// Excluded repos should not appear in the packages.json5 cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_excluded_repos_not_in_cache() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("cache_repo_a");
    let repo_b = started.peppy_dirs.root().join("cache_repo_b");
    create_node_dir(&repo_a, "cached_node", "1.0.0");
    create_node_dir(&repo_b, "excluded_node", "1.0.0");

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

    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    let content = std::fs::read_to_string(&cache_path).expect("read cache");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse cache");
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
    create_node_dir(&repo, "fs_node", "1.0.0");

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

    let excluded_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| f.excluded).collect();
    assert_eq!(excluded_feedbacks.len(), 1);
    assert_eq!(excluded_feedbacks[0].source_type, RepoSourceKind::Git);

    let discovered_feedbacks: Vec<&RepoRefreshFeedback> =
        result.feedbacks.iter().filter(|f| !f.excluded).collect();
    assert_eq!(discovered_feedbacks.len(), 1);
    assert_eq!(discovered_feedbacks[0].node_name, "fs_node");
}
