mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use core_node::encoding::{RepoAddRequest, RepoAddResponse};
use core_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

async fn send_repo_add(started: &StartedCoreNode, request: &RepoAddRequest) -> RepoAddResponse {
    let payload = request.encode().expect("encode should succeed");
    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        names::REPO_ADD,
        Some(&started.core_node_name),
        None,
        payload,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_add poll should succeed");
    RepoAddResponse::decode(&response.payload()).expect("decode should succeed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_url_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/packages"),
    )
    .await;
    assert!(resp.success, "repo_add should succeed");
    assert!(resp.error_message.is_empty());

    // Verify the file was created with the home dir default + the new entry
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    // First entry is the home dir default (fs type) with id 1
    assert_eq!(repos[0]["type"], "fs");
    assert_eq!(repos[0]["id"], 1);

    // Last entry is the one we just added, with the next available id
    let last = repos.last().unwrap();
    assert_eq!(last["type"], "url");
    assert_eq!(last["url"], "https://example.com/packages");
    assert_eq!(
        last["id"], 3,
        "added entry should get the next available id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_git(
            "https://github.com/example/repo.git",
            Some("main".to_string()),
        ),
    )
    .await;
    assert!(resp.success, "repo_add should succeed");

    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    let last = repos.last().unwrap();
    assert_eq!(last["type"], "git");
    assert_eq!(last["url"], "https://github.com/example/repo.git");
    assert_eq!(last["ref"], "main");
    assert_eq!(
        last["id"], 3,
        "added entry should get the next available id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_fs_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = send_repo_add(&started, &RepoAddRequest::new_fs("/tmp/my-local-repo")).await;
    assert!(resp.success, "repo_add should succeed");

    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");

    let last = repos.last().unwrap();
    assert_eq!(last["type"], "fs");
    assert_eq!(last["path"], "/tmp/my-local-repo");
    assert_eq!(
        last["id"], 3,
        "added entry should get the next available id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_duplicate_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let request = RepoAddRequest::new_url("https://example.com/packages");

    // First add should succeed
    let resp = send_repo_add(&started, &request).await;
    assert!(resp.success);

    // Second add of the same URL should fail
    let resp = send_repo_add(&started, &request).await;
    assert!(!resp.success, "duplicate add should fail");
    assert!(
        resp.error_message.contains("already exists"),
        "error should mention 'already exists', got: {}",
        resp.error_message
    );
}

fn write_repositories_json5(started: &StartedCoreNode, content: &str) {
    let conf_dir = started.peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("repositories.json5"), content).expect("write repos file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_fails_when_duplicate_ids_in_file() {
    let started = start_core_node_with_mock_messenger().await;

    // Simulate a user manually editing the file and introducing duplicate ids
    write_repositories_json5(
        &started,
        r#"[
            { "id": 1, "type": "fs", "path": "/repo-a" },
            { "id": 1, "type": "fs", "path": "/repo-b" }
        ]"#,
    );

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/new"),
    )
    .await;
    assert!(
        !resp.success,
        "repo_add should fail when duplicate ids exist"
    );
    assert!(
        resp.error_message.contains("duplicate repository id"),
        "error should mention duplicate id, got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_assigns_id_after_manual_entry() {
    let started = start_core_node_with_mock_messenger().await;

    // Pre-populate with a manually-added entry using a high id
    write_repositories_json5(
        &started,
        r#"[{ "id": 42, "type": "fs", "path": "/manual-repo" }]"#,
    );

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/packages"),
    )
    .await;
    assert!(resp.success, "repo_add should succeed");

    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
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
