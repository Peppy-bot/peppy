mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use config::consts::NODE_CONFIG_FILE;
use core_node_api::encoding::{RepoRemoveRequest, RepoRemoveResponse};
use peppylib::core_node::transport::poll_repo_remove;
use std::time::Duration;

async fn send_repo_remove(
    started: &StartedCoreNode,
    request: &RepoRemoveRequest,
) -> RepoRemoveResponse {
    poll_repo_remove(
        request,
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_remove poll should succeed")
}

fn write_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("repositories.json5"), content).expect("write repos file");
}

fn write_packages_cache(started: &StartedCoreNode, content: &str) {
    let cache_dir = started.peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");
    std::fs::write(cache_dir.join("packages.json5"), content).expect("write cache file");
}

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

fn create_node_dir(base: &std::path::Path, name: &str, tag: &str) -> std::path::PathBuf {
    let dir = base.join(format!("{name}_{tag}"));
    std::fs::create_dir_all(&dir).expect("create node dir");
    std::fs::write(dir.join(NODE_CONFIG_FILE), minimal_peppy_json5(name, tag))
        .expect("write peppy.json5");
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_fs_repo_succeeds() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_path = "/tmp/my-local-repo";
    write_repositories_json5(
        &started,
        &format!(r#"[{{ "id": 1, "type": "fs", "path": "{repo_path}" }}]"#),
    );

    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(1)).await;
    assert!(resp.success, "repo_remove should succeed");
    assert!(resp.error_message.is_empty());

    // Verify the entry was removed from repositories.json5
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert!(repos.is_empty(), "repos should be empty after removal");

    // Cache refresh is triggered for all repo types (including fs)
    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    assert!(
        cache_path.exists(),
        "packages.json5 cache should exist after fs repo removal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_git_repo_succeeds_and_triggers_refresh() {
    let started = start_core_node_with_mock_messenger().await;

    let git_url = "https://github.com/example/repo.git";
    let fs_repo_dir = started.peppy_dirs.root().join("test_repo");
    create_node_dir(&fs_repo_dir, "local_sensor", "1.0.0");

    // Write repos with both an fs entry and a git entry
    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "git", "url": "{git_url}", "ref": "main" }}]"#,
            fs_repo_dir.display()
        ),
    );

    // Pre-populate stale cache with nodes from the git repo
    write_packages_cache(
        &started,
        &format!(
            r#"[{{
  "node_name": "git_sensor",
  "node_tag": "1.0.0",
  "source_type": "git",
  "source_uri": "{git_url}",
  "path": "nodes/git_sensor"
}}]"#
        ),
    );

    // Remove the git repo by its id (2)
    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(2)).await;
    assert!(
        resp.success,
        "repo_remove should succeed, got error: {}",
        resp.error_message
    );

    // Verify the git entry was removed from repositories.json5
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert_eq!(repos.len(), 1, "only the fs entry should remain");
    assert_eq!(repos[0]["type"], "fs");

    // Verify refresh was triggered: packages.json5 should be updated.
    // Since the git repo was removed, cache should no longer contain git_sensor.
    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    assert!(
        cache_path.exists(),
        "packages.json5 cache should exist after refresh"
    );
    let cache_content = std::fs::read_to_string(&cache_path).expect("read cache file");
    let cached: Vec<serde_json::Value> =
        serde_json::from_str(&cache_content).expect("parse cache as JSON");
    assert!(
        !cached
            .iter()
            .any(|n| n.get("source_uri").and_then(|v| v.as_str()) == Some(git_url)),
        "cache should not contain nodes from the removed git repo"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_url_repo_succeeds_and_triggers_refresh() {
    let started = start_core_node_with_mock_messenger().await;

    let url = "https://example.com/packages";
    let fs_repo_dir = started.peppy_dirs.root().join("test_repo");
    create_node_dir(&fs_repo_dir, "local_sensor", "1.0.0");

    write_repositories_json5(
        &started,
        &format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}, {{ "id": 2, "type": "url", "url": "{url}" }}]"#,
            fs_repo_dir.display()
        ),
    );

    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(2)).await;
    assert!(
        resp.success,
        "repo_remove should succeed, got error: {}",
        resp.error_message
    );

    // Verify the url entry was removed
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert_eq!(repos.len(), 1, "only the fs entry should remain");
    assert_eq!(repos[0]["type"], "fs");

    // Verify refresh was triggered (cache file should be written)
    let cache_path = started.peppy_dirs.cache_dir().join("packages.json5");
    assert!(
        cache_path.exists(),
        "packages.json5 cache should exist after refresh"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_nonexistent_id_fails() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(
        &started,
        r#"[{ "id": 1, "type": "fs", "path": "/tmp/existing-repo" }]"#,
    );

    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(999)).await;
    assert!(!resp.success, "repo_remove should fail for unknown id");
    assert!(
        resp.error_message.contains("not found"),
        "error should mention 'not found', got: {}",
        resp.error_message
    );

    // Verify the original repos file is unchanged
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert_eq!(repos.len(), 1, "repos should be unchanged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_from_empty_repos_fails() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, "[]");

    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(1)).await;
    assert!(!resp.success, "repo_remove should fail on empty list");
    assert!(
        resp.error_message.contains("not found"),
        "error should mention 'not found', got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_fails_when_duplicate_ids_in_file() {
    let started = start_core_node_with_mock_messenger().await;

    // Simulate a user manually editing repositories.json5 and introducing duplicate ids
    write_repositories_json5(
        &started,
        r#"[
            { "id": 1, "type": "fs", "path": "/repo-a" },
            { "id": 1, "type": "fs", "path": "/repo-b" }
        ]"#,
    );

    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(1)).await;
    assert!(
        !resp.success,
        "repo_remove should fail when duplicate ids exist"
    );
    assert!(
        resp.error_message.contains("duplicate repository id"),
        "error should mention duplicate id, got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_verifies_id_on_manually_added_entry() {
    let started = start_core_node_with_mock_messenger().await;

    // Simulate a user manually adding an entry with a specific id
    write_repositories_json5(
        &started,
        r#"[
            { "id": 10, "type": "fs", "path": "/repo-a" },
            { "id": 20, "type": "fs", "path": "/repo-b" },
            { "id": 30, "type": "fs", "path": "/repo-c" }
        ]"#,
    );

    // Remove the middle entry by its specific id
    let resp = send_repo_remove(&started, &RepoRemoveRequest::new(20)).await;
    assert!(
        resp.success,
        "repo_remove should succeed, got error: {}",
        resp.error_message
    );

    // Verify the correct entry was removed and ids are preserved
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert_eq!(repos.len(), 2, "two entries should remain");
    assert_eq!(repos[0]["id"], 10);
    assert_eq!(repos[0]["path"], "/repo-a");
    assert_eq!(repos[1]["id"], 30);
    assert_eq!(repos[1]["path"], "/repo-c");
}
