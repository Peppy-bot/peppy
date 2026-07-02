mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use core_node::repositories_list_path;
use core_node_api::encoding::{RepoAddRequest, RepoAddResponse};
use peppylib::core_node::transport::poll_repo_add;
use std::time::Duration;

async fn send_repo_add(started: &StartedCoreNode, request: &RepoAddRequest) -> RepoAddResponse {
    poll_repo_add(
        request,
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_add poll should succeed")
}

/// Assert the most recently appended repo got the next-available id:
/// exactly `max(other_ids) + 1`. Avoids hardcoding a specific value, which
/// goes stale every time a default is added to the shipped
/// `default_repositories.json5`, and catches regressions where ids skip ahead.
fn assert_last_got_next_id(repos: &[serde_json::Value]) {
    let last_id = repos
        .last()
        .and_then(|r| r["id"].as_u64())
        .expect("last repo should have an integer id");
    let max_other = repos[..repos.len() - 1]
        .iter()
        .filter_map(|r| r["id"].as_u64())
        .max()
        .unwrap_or(0);
    assert_eq!(
        last_id,
        max_other + 1,
        "added entry should get exactly max_other + 1 (got {last_id}, max other {max_other})"
    );
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

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    // Last entry is the one we just added, with the next available id
    let last = repos.last().unwrap();
    assert_eq!(last["type"], "url");
    assert_eq!(last["url"], "https://example.com/packages");
    assert_last_got_next_id(&repos);
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

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let last = repos.last().unwrap();
    assert_eq!(last["type"], "git");
    assert_eq!(last["url"], "https://github.com/example/repo.git");
    assert_eq!(last["ref"], "main");
    assert_last_got_next_id(&repos);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_fs_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = send_repo_add(&started, &RepoAddRequest::new_fs("/tmp/my-local-repo")).await;
    assert!(resp.success, "repo_add should succeed");

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let last = repos.last().unwrap();
    assert_eq!(last["type"], "fs");
    assert_eq!(last["path"], "/tmp/my-local-repo");
    assert_last_got_next_id(&repos);
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

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_assigns_min_minus_one() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(
        &started,
        r#"[
            { "id": 1000, "type": "fs", "path": "/a" },
            { "id": 1001, "type": "fs", "path": "/b" }
        ]"#,
    );

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/packages").with_top(true),
    )
    .await;
    assert!(resp.success, "repo_add with top should succeed");

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let added = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/packages")
        .expect("added entry should be present");
    assert_eq!(added["id"], 999, "top=true should assign min(existing)-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_sorts_first() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(
        &started,
        r#"[
            { "id": 1000, "type": "fs", "path": "/a" },
            { "id": 1001, "type": "fs", "path": "/b" }
        ]"#,
    );

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/packages").with_top(true),
    )
    .await;
    assert!(resp.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    // Lower id = higher priority once the list is sorted on read.
    let lowest = repos
        .iter()
        .min_by_key(|e| e["id"].as_u64().unwrap_or(u64::MAX))
        .expect("file must not be empty");
    assert_eq!(lowest["url"], "https://example.com/packages");
    assert_eq!(lowest["id"], 999);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_on_empty_uses_default_floor() {
    let started = start_core_node_with_mock_messenger().await;
    write_repositories_json5(&started, "[]");

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/x").with_top(true),
    )
    .await;
    assert!(resp.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    assert_eq!(repos.len(), 1);
    assert_eq!(
        repos[0]["id"], 1000,
        "empty list + top=true should fall back to the 1000 floor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_no_top_on_empty_uses_default_floor() {
    let started = start_core_node_with_mock_messenger().await;
    write_repositories_json5(&started, "[]");

    let resp = send_repo_add(&started, &RepoAddRequest::new_url("https://example.com/x")).await;
    assert!(resp.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    assert_eq!(repos.len(), 1);
    assert_eq!(
        repos[0]["id"], 1000,
        "empty list + top=false should also fall back to the 1000 floor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_fails_when_min_is_zero() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, r#"[{ "id": 0, "type": "fs", "path": "/a" }]"#);

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/x").with_top(true),
    )
    .await;
    assert!(!resp.success, "top=true on min=0 must fail");
    assert!(
        resp.error_message.contains("underflow"),
        "error should mention underflow, got: {}",
        resp.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_sequential_adds_keep_decreasing() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, r#"[{ "id": 1000, "type": "fs", "path": "/a" }]"#);

    let resp1 = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/first").with_top(true),
    )
    .await;
    assert!(resp1.success);

    let resp2 = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/second").with_top(true),
    )
    .await;
    assert!(resp2.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let first = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/first")
        .unwrap();
    let second = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/second")
        .unwrap();
    assert_eq!(first["id"], 999);
    assert_eq!(second["id"], 998);

    // The second add should own the new minimum id.
    let lowest = repos
        .iter()
        .min_by_key(|e| e["id"].as_u64().unwrap_or(u64::MAX))
        .unwrap();
    assert_eq!(lowest["url"], "https://example.com/second");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_false_preserves_max_plus_one() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, r#"[{ "id": 1000, "type": "fs", "path": "/a" }]"#);

    let resp = send_repo_add(
        &started,
        &RepoAddRequest::new_url("https://example.com/x").with_top(false),
    )
    .await;
    assert!(resp.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let added = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/x")
        .unwrap();
    assert_eq!(added["id"], 1001, "top=false must keep max+1 behavior");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_same_git_url_different_refs_are_distinct() {
    // Regression for RepoSource::identity() collapsing git entries by url
    // alone: adding the same clone URL with different refs must produce two
    // distinct entries instead of rejecting the second as a duplicate.
    let started = start_core_node_with_mock_messenger().await;

    let resp_main = send_repo_add(
        &started,
        &RepoAddRequest::new_git("https://github.com/example/repo.git", Some("main".into())),
    )
    .await;
    assert!(resp_main.success, "first add should succeed");

    let resp_dev = send_repo_add(
        &started,
        &RepoAddRequest::new_git("https://github.com/example/repo.git", Some("dev".into())),
    )
    .await;
    assert!(
        resp_dev.success,
        "second add with different ref should succeed, got error: {}",
        resp_dev.error_message
    );

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let git_entries: Vec<&serde_json::Value> = repos
        .iter()
        .filter(|e| e["type"] == "git" && e["url"] == "https://github.com/example/repo.git")
        .collect();
    assert_eq!(
        git_entries.len(),
        2,
        "both ref variants should be persisted"
    );
    let refs: Vec<&str> = git_entries
        .iter()
        .map(|e| e["ref"].as_str().unwrap_or(""))
        .collect();
    assert!(refs.contains(&"main"));
    assert!(refs.contains(&"dev"));
}
