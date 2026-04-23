mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use config::consts::NODE_CONFIG_FILE;
use core_node::names;
use core_node_api::encoding::{RepoListRequest, RepoListResponse, RepoSourceKind};
use peppylib::ServiceMessenger;
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

/// Write a packages.json5 cache file in the cache_dir of the started core node.
fn write_packages_cache(started: &StartedCoreNode, content: &str) {
    let cache_dir = started.peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");
    std::fs::write(cache_dir.join("packages.json5"), content).expect("write cache file");
}

/// Create a directory with a valid peppy.json5 inside it.
fn create_node_dir(base: &std::path::Path, name: &str, tag: &str) -> std::path::PathBuf {
    let dir = base.join(format!("{name}_{tag}"));
    std::fs::create_dir_all(&dir).expect("create node dir");
    std::fs::write(dir.join(NODE_CONFIG_FILE), minimal_peppy_json5(name, tag))
        .expect("write peppy.json5");
    dir
}

async fn send_repo_list(started: &StartedCoreNode) -> RepoListResponse {
    let request = RepoListRequest;
    let payload = request.encode().expect("encode should succeed");
    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        names::REPO_LIST,
        Some(&started.core_node_name),
        None,
        payload,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_list poll should succeed");
    RepoListResponse::decode(&response.payload()).expect("decode should succeed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_default_repos_creates_repositories_file() {
    let started = start_core_node_with_mock_messenger().await;

    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    assert!(
        !repos_path.exists(),
        "repositories.json5 should not exist before first repo command"
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success, "repo_list should succeed");
    assert!(resp.error_message.is_none());

    assert!(
        repos_path.exists(),
        "repositories.json5 should be created by repo list"
    );

    let content = std::fs::read_to_string(&repos_path).expect("read created file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse created file");
    assert_eq!(repos.len(), 1, "default file should contain 1 entry");
    assert_eq!(repos[0].get("type").unwrap().as_str().unwrap(), "git");
    assert_eq!(
        repos[0].get("url").unwrap().as_str().unwrap(),
        "https://github.com/Peppy-bot/nodes_hub"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_finds_nodes_in_fs_repo() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("test_repo");
    create_node_dir(&repo_dir, "my_sensor", "1.0.0");
    create_node_dir(&repo_dir, "my_actuator", "2.0.0");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success, "repo_list should succeed");
    assert_eq!(resp.nodes.len(), 2, "should find 2 nodes");

    let names: Vec<&str> = resp.nodes.iter().map(|n| n.node_name.as_str()).collect();
    assert!(names.contains(&"my_sensor"), "should contain my_sensor");
    assert!(names.contains(&"my_actuator"), "should contain my_actuator");

    let expected_label = repo_dir.to_string_lossy().into_owned();
    for node in &resp.nodes {
        assert_eq!(node.source_type, RepoSourceKind::Fs);
        assert_eq!(node.repo_id, 1);
        assert_eq!(node.repo_label, expected_label);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_reads_git_nodes_from_cache() {
    let started = start_core_node_with_mock_messenger().await;

    let git_url = "https://github.com/example/repo.git";

    // Write a repositories.json5 with a git repo
    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "git", "url": git_url, "ref": "main" }
        ]))
        .unwrap(),
    );

    // Write a packages.json5 cache with nodes from that git repo
    write_packages_cache(
        &started,
        &serde_json::to_string(&serde_json::json!([
            {
                "node_name": "git_sensor",
                "node_tag": "1.0.0",
                "source_type": "git",
                "source_uri": git_url,
                "resolved_ref": "main",
                "path": "nodes/git_sensor"
            },
            {
                "node_name": "git_actuator",
                "node_tag": "2.0.0",
                "source_type": "git",
                "source_uri": git_url,
                "resolved_ref": "main",
                "path": "nodes/git_actuator"
            }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success, "repo_list should succeed");
    assert_eq!(resp.nodes.len(), 2, "should find 2 git nodes from cache");

    let expected_label = format!("{git_url} (ref: main)");

    let sensor = resp
        .nodes
        .iter()
        .find(|n| n.node_name == "git_sensor")
        .expect("should find git_sensor");
    assert_eq!(sensor.node_tag, "1.0.0");
    assert_eq!(sensor.source_type, RepoSourceKind::Git);
    assert_eq!(sensor.path, "nodes/git_sensor");
    assert_eq!(sensor.repo_id, 1);
    assert_eq!(sensor.repo_label, expected_label);

    let actuator = resp
        .nodes
        .iter()
        .find(|n| n.node_name == "git_actuator")
        .expect("should find git_actuator");
    assert_eq!(actuator.node_tag, "2.0.0");
    assert_eq!(actuator.source_type, RepoSourceKind::Git);
    assert_eq!(actuator.path, "nodes/git_actuator");
    assert_eq!(actuator.repo_id, 1);
    assert_eq!(actuator.repo_label, expected_label);
}

/// When two FS repositories provide the same node, the list should contain both
/// entries — the first as non-duplicate and the second marked as duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_marks_cross_repo_duplicates_fs() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("list_dup_a");
    let repo_b = started.peppy_dirs.root().join("list_dup_b");
    create_node_dir(&repo_a, "shared", "1.0.0");
    create_node_dir(&repo_b, "shared", "1.0.0");
    create_node_dir(&repo_b, "unique_b", "1.0.0");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_a.to_string_lossy() },
            { "id": 2, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(
        resp.nodes.len(),
        3,
        "should contain 3 entries (shared from a, shared from b as dup, unique_b)"
    );

    let shared_entries: Vec<_> = resp
        .nodes
        .iter()
        .filter(|n| n.node_name == "shared")
        .collect();
    assert_eq!(shared_entries.len(), 2, "shared should appear twice");

    let primary = shared_entries
        .iter()
        .find(|n| !n.duplicate)
        .expect("primary");
    assert!(
        primary.path.contains("list_dup_a"),
        "primary should come from repo_a"
    );
    assert_eq!(primary.repo_id, 1);
    assert_eq!(primary.repo_label, repo_a.to_string_lossy());

    let dup = shared_entries
        .iter()
        .find(|n| n.duplicate)
        .expect("duplicate");
    assert!(
        dup.path.contains("list_dup_b"),
        "duplicate should come from repo_b"
    );
    assert_eq!(dup.repo_id, 2);
    assert_eq!(dup.repo_label, repo_b.to_string_lossy());
    assert_ne!(primary.repo_label, dup.repo_label);

    let unique = resp
        .nodes
        .iter()
        .find(|n| n.node_name == "unique_b")
        .expect("unique_b");
    assert!(!unique.duplicate, "unique_b should not be a duplicate");
}

/// When a git-cached node overlaps with a local FS node, the git entry should
/// be marked as duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_marks_git_duplicate_of_fs() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo_dir, "overlapping", "1.0.0");

    let git_url = "https://github.com/example/nodes.git";

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() },
            { "id": 2, "type": "git", "url": git_url }
        ]))
        .unwrap(),
    );

    // Cache has the same node from git
    write_packages_cache(
        &started,
        &serde_json::to_string(&serde_json::json!([{
            "node_name": "overlapping",
            "node_tag": "1.0.0",
            "source_type": "git",
            "source_uri": git_url,
            "resolved_ref": "main",
            "path": "nodes/overlapping"
        }]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(
        resp.nodes.len(),
        2,
        "should contain both the fs and git entries"
    );

    let fs_entry = resp
        .nodes
        .iter()
        .find(|n| n.source_type == RepoSourceKind::Fs)
        .expect("fs entry");
    assert!(!fs_entry.duplicate, "fs entry should be primary");
    assert_eq!(fs_entry.repo_id, 1);
    assert_eq!(fs_entry.repo_label, repo_dir.to_string_lossy());

    let git_entry = resp
        .nodes
        .iter()
        .find(|n| n.source_type == RepoSourceKind::Git)
        .expect("git entry");
    assert!(
        git_entry.duplicate,
        "git entry should be marked as duplicate"
    );
    assert_eq!(git_entry.repo_id, 2);
    assert_eq!(git_entry.repo_label, format!("{git_url} (ref: main)"));
}

/// Write an excluded_repositories.json5 file in the conf_dir.
fn write_excluded_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("excluded_repositories.json5"), content)
        .expect("write excluded repos file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_empty_repos_file() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, "[]");

    let resp = send_repo_list(&started).await;
    assert!(resp.success, "repo_list should succeed");
    assert!(resp.nodes.is_empty(), "should have no nodes");
}

/// An excluded FS repo should not appear in the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_excludes_fs_repo() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("repo_a");
    let repo_b = started.peppy_dirs.root().join("repo_b");
    create_node_dir(&repo_a, "node_a", "1.0.0");
    create_node_dir(&repo_b, "node_b", "1.0.0");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_a.to_string_lossy() },
            { "id": 2, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );
    write_excluded_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(resp.nodes.len(), 1, "only node_a should be listed");
    assert_eq!(resp.nodes[0].node_name, "node_a");
}

/// An excluded subdirectory within an FS repo should be pruned from the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_excludes_fs_subdirectory() {
    let started = start_core_node_with_mock_messenger().await;

    let repo = started.peppy_dirs.root().join("mixed_repo");
    create_node_dir(&repo, "keep_node", "1.0.0");
    create_node_dir(&repo, "secret_node", "1.0.0");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo.to_string_lossy() }
        ]))
        .unwrap(),
    );
    write_excluded_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo.join("secret_node_1.0.0").to_string_lossy() }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(resp.nodes.len(), 1, "only keep_node should be listed");
    assert_eq!(resp.nodes[0].node_name, "keep_node");
}

/// An excluded git repo should not appear in the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_excludes_git_repo() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    create_node_dir(&repo_dir, "fs_node", "1.0.0");

    let git_url = "https://github.com/example/excluded.git";

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() },
            { "id": 2, "type": "git", "url": git_url }
        ]))
        .unwrap(),
    );
    write_packages_cache(
        &started,
        &serde_json::to_string(&serde_json::json!([{
            "node_name": "git_node",
            "node_tag": "1.0.0",
            "source_type": "git",
            "source_uri": git_url,
            "path": "nodes/git_node"
        }]))
        .unwrap(),
    );
    write_excluded_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "git", "url": git_url }
        ]))
        .unwrap(),
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(resp.nodes.len(), 1, "only fs_node should be listed");
    assert_eq!(resp.nodes[0].node_name, "fs_node");
    assert_eq!(resp.nodes[0].source_type, RepoSourceKind::Fs);
}
