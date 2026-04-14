mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use core_node::encoding::{RepoExcludeRequest, RepoExcludeResponse};
use core_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

async fn send_repo_exclude(
    started: &StartedCoreNode,
    request: &RepoExcludeRequest,
) -> RepoExcludeResponse {
    let payload = request.encode().expect("encode should succeed");
    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        names::REPO_EXCLUDE,
        Some(&started.core_node_name),
        None,
        payload,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_exclude poll should succeed");
    RepoExcludeResponse::decode(&response.payload()).expect("decode should succeed")
}

fn write_excluded_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("excluded_repositories.json5"), content)
        .expect("write excluded repos file");
}

/// Pre-writes an empty `repositories.json5` so the exclude handler's
/// post-response refresh step does not fall back to the default template,
/// which points at the real user `$HOME` and would make `process_refresh`
/// walk the entire home directory (causing 5s poll timeouts).
fn write_empty_repositories_json5(started: &StartedCoreNode) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("repositories.json5"), "[]").expect("write repos file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_url_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    let resp = send_repo_exclude(
        &started,
        &RepoExcludeRequest::new_url("https://example.com/packages"),
    )
    .await;
    assert!(resp.success, "repo_exclude should succeed");
    assert!(resp.error_message.is_empty());

    // Verify the file was created with the new entry
    let repos_path = started
        .peppy_dirs
        .conf_dir()
        .join("excluded_repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read excluded repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    assert_eq!(repos.len(), 1);
    let entry = &repos[0];
    assert_eq!(entry["type"], "url");
    assert_eq!(entry["url"], "https://example.com/packages");
    assert_eq!(entry["id"], 1, "first entry should get id 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    let resp = send_repo_exclude(
        &started,
        &RepoExcludeRequest::new_git(
            "https://github.com/example/repo.git",
            Some("main".to_string()),
        ),
    )
    .await;
    assert!(resp.success, "repo_exclude should succeed");

    let repos_path = started
        .peppy_dirs
        .conf_dir()
        .join("excluded_repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read excluded repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    assert_eq!(repos.len(), 1);
    let entry = &repos[0];
    assert_eq!(entry["type"], "git");
    assert_eq!(entry["url"], "https://github.com/example/repo.git");
    assert_eq!(entry["ref"], "main");
    assert_eq!(entry["id"], 1, "first entry should get id 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_fs_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    let resp = send_repo_exclude(&started, &RepoExcludeRequest::new_fs("/tmp/my-local-repo")).await;
    assert!(resp.success, "repo_exclude should succeed");

    let repos_path = started
        .peppy_dirs
        .conf_dir()
        .join("excluded_repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read excluded repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    assert_eq!(repos.len(), 1);
    let entry = &repos[0];
    assert_eq!(entry["type"], "fs");
    assert_eq!(entry["path"], "/tmp/my-local-repo");
    assert_eq!(entry["id"], 1, "first entry should get id 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_duplicate_fails() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    let request = RepoExcludeRequest::new_url("https://example.com/packages");

    // First exclude should succeed
    let resp = send_repo_exclude(&started, &request).await;
    assert!(resp.success);

    // Second exclude of the same URL should fail
    let resp = send_repo_exclude(&started, &request).await;
    assert!(!resp.success, "duplicate exclude should fail");
    assert!(
        resp.error_message.contains("already exists"),
        "error should mention 'already exists', got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_fails_when_duplicate_ids_in_file() {
    let started = start_core_node_with_mock_messenger().await;

    // Simulate a user manually editing the file and introducing duplicate ids
    write_excluded_repositories_json5(
        &started,
        r#"[
            { "id": 1, "type": "fs", "path": "/repo-a" },
            { "id": 1, "type": "fs", "path": "/repo-b" }
        ]"#,
    );

    let resp = send_repo_exclude(
        &started,
        &RepoExcludeRequest::new_url("https://example.com/new"),
    )
    .await;
    assert!(
        !resp.success,
        "repo_exclude should fail when duplicate ids exist"
    );
    assert!(
        resp.error_message.contains("duplicate repository id"),
        "error should mention duplicate id, got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_assigns_id_after_manual_entry() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    // Pre-populate with a manually-added entry using a high id
    write_excluded_repositories_json5(
        &started,
        r#"[{ "id": 42, "type": "fs", "path": "/manual-repo" }]"#,
    );

    let resp = send_repo_exclude(
        &started,
        &RepoExcludeRequest::new_url("https://example.com/packages"),
    )
    .await;
    assert!(resp.success, "repo_exclude should succeed");

    let repos_path = started
        .peppy_dirs
        .conf_dir()
        .join("excluded_repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read excluded repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    assert_eq!(repos.len(), 2);

    // The manual entry should keep its id
    let manual = repos.iter().find(|e| e["path"] == "/manual-repo").unwrap();
    assert_eq!(manual["id"], 42);

    // The new entry should get id 43 (max existing + 1)
    let added = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/packages")
        .unwrap();
    assert_eq!(
        added["id"], 43,
        "new entry should get max(existing ids) + 1"
    );
}
