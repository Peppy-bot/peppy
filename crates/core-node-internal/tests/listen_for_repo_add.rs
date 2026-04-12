mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use core_node::encoding::{RepoAddRequest, RepoAddResponse};
use core_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_url_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let request = RepoAddRequest::new_url("https://example.com/packages");
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
    .expect("repo_add request should succeed");

    let resp = RepoAddResponse::decode(&response.payload()).expect("decode should succeed");
    assert!(resp.success, "repo_add should succeed");
    assert!(resp.error_message.is_empty());

    // Verify the file was created with correct contents
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["type"], "url");
    assert_eq!(repos[0]["url"], "https://example.com/packages");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let request = RepoAddRequest::new_git(
        "https://github.com/example/repo.git",
        Some("main".to_string()),
    );
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
    .expect("repo_add request should succeed");

    let resp = RepoAddResponse::decode(&response.payload()).expect("decode should succeed");
    assert!(resp.success, "repo_add should succeed");

    // Verify the file contents
    let repos_path = started.peppy_dirs.conf_dir().join("repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("parse repos as JSON");
    println!("Wolo = {}", repos_path.display());
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["type"], "git");
    assert_eq!(repos[0]["url"], "https://github.com/example/repo.git");
    assert_eq!(repos[0]["ref"], "main");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_duplicate_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let request = RepoAddRequest::new_url("https://example.com/packages");

    // First add should succeed
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
    .expect("first repo_add should succeed");
    let resp = RepoAddResponse::decode(&response.payload()).expect("decode should succeed");
    assert!(resp.success);

    // Second add of the same URL should fail
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
    .expect("second repo_add should complete");
    let resp = RepoAddResponse::decode(&response.payload()).expect("decode should succeed");
    assert!(!resp.success, "duplicate add should fail");
    assert!(
        resp.error_message.contains("already exists"),
        "error should mention 'already exists', got: {}",
        resp.error_message
    );
}
