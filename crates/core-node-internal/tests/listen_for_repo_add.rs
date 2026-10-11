mod common;

use common::{
    StartedCoreNode, poll_core_node, start_core_node_with_mock_messenger, write_repositories_json5,
};
use core_node::repositories_list_path;
use core_node_api::encoding::{GitRepoRef, RepoAddRequest};

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
async fn listen_for_repo_add_second_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead),
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
    assert_eq!(last["type"], "git");
    assert_eq!(last["url"], "https://example.com/packages.git");
    assert_last_got_next_id(&repos);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git(
            "https://github.com/example/repo.git",
            GitRepoRef::Named("main".to_string()),
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

    let resp = poll_core_node(&started, &RepoAddRequest::new_fs("/tmp/my-local-repo")).await;
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

    let request =
        RepoAddRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead);

    // First add should succeed
    let resp = poll_core_node(&started, &request).await;
    assert!(resp.success);

    // Second add of the same URL should fail
    let resp = poll_core_node(&started, &request).await;
    assert!(!resp.success, "duplicate add should fail");
    assert!(
        resp.error_message.contains("already exists"),
        "error should mention 'already exists', got: {}",
        resp.error_message
    );
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

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/new.git", GitRepoRef::RemoteHead),
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

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead),
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
        .find(|e| e["url"] == "https://example.com/packages.git")
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

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead)
            .with_top(true),
    )
    .await;
    assert!(resp.success, "repo_add with top should succeed");

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let added = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/packages.git")
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

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead)
            .with_top(true),
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
    assert_eq!(lowest["url"], "https://example.com/packages.git");
    assert_eq!(lowest["id"], 999);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_on_empty_uses_default_floor() {
    let started = start_core_node_with_mock_messenger().await;
    write_repositories_json5(&started, "[]");

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/x.git", GitRepoRef::RemoteHead)
            .with_top(true),
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

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/x.git", GitRepoRef::RemoteHead),
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
        "empty list + top=false should also fall back to the 1000 floor"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_fails_when_min_is_zero() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, r#"[{ "id": 0, "type": "fs", "path": "/a" }]"#);

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/x.git", GitRepoRef::RemoteHead)
            .with_top(true),
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

    let resp1 = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/first.git", GitRepoRef::RemoteHead)
            .with_top(true),
    )
    .await;
    assert!(resp1.success);

    let resp2 = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/second.git", GitRepoRef::RemoteHead)
            .with_top(true),
    )
    .await;
    assert!(resp2.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let first = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/first.git")
        .unwrap();
    let second = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/second.git")
        .unwrap();
    assert_eq!(first["id"], 999);
    assert_eq!(second["id"], 998);

    // The second add should own the new minimum id.
    let lowest = repos
        .iter()
        .min_by_key(|e| e["id"].as_u64().unwrap_or(u64::MAX))
        .unwrap();
    assert_eq!(lowest["url"], "https://example.com/second.git");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_top_false_preserves_max_plus_one() {
    let started = start_core_node_with_mock_messenger().await;

    write_repositories_json5(&started, r#"[{ "id": 1000, "type": "fs", "path": "/a" }]"#);

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_git("https://example.com/x.git", GitRepoRef::RemoteHead)
            .with_top(false),
    )
    .await;
    assert!(resp.success);

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let added = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/x.git")
        .unwrap();
    assert_eq!(added["id"], 1001, "top=false must keep max+1 behavior");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_same_git_url_different_refs_are_distinct() {
    // Git entries are identified by url and ref together: the same clone URL
    // with different refs gives two distinct entries, not a duplicate
    // rejection.
    let started = start_core_node_with_mock_messenger().await;

    let resp_main = poll_core_node(
        &started,
        &RepoAddRequest::new_git(
            "https://github.com/example/repo.git",
            GitRepoRef::Named("main".to_string()),
        ),
    )
    .await;
    assert!(resp_main.success, "first add should succeed");

    let resp_dev = poll_core_node(
        &started,
        &RepoAddRequest::new_git(
            "https://github.com/example/repo.git",
            GitRepoRef::Named("dev".to_string()),
        ),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_explicit_id_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_fs("/tmp/private-hub").with_id(2000),
    )
    .await;
    assert!(resp.success, "repo_add with explicit id should succeed");

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");

    let added = repos
        .iter()
        .find(|e| e["path"] == "/tmp/private-hub")
        .expect("added entry should be persisted");
    assert_eq!(added["id"], 2000, "entry must carry the pinned id");
    assert_eq!(
        repos.iter().filter(|e| e["id"] == 2000).count(),
        1,
        "exactly one entry may hold the pinned id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_explicit_id_collision_fails() {
    let started = start_core_node_with_mock_messenger().await;

    // 1000 is the id of the first bundled default; claiming it would merge
    // the new repository's entries under that default's owner.
    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_fs("/tmp/private-hub").with_id(1000),
    )
    .await;
    assert!(!resp.success, "add pinning a taken id should fail");
    assert!(
        resp.error_message.contains("id 1000 is already in use"),
        "error should name the taken id, got: {}",
        resp.error_message
    );

    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse repos as JSON5");
    assert!(
        repos.iter().all(|e| e["path"] != "/tmp/private-hub"),
        "refused entry must not be persisted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_explicit_id_with_top_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let resp = poll_core_node(
        &started,
        &RepoAddRequest::new_fs("/tmp/private-hub")
            .with_id(2000)
            .with_top(true),
    )
    .await;
    assert!(!resp.success, "id + top should fail");
    assert!(
        resp.error_message
            .contains("cannot combine an explicit id with top priority"),
        "error should explain the conflict, got: {}",
        resp.error_message
    );
}

/// Reads repositories.json5 back and asserts that it still parses and holds
/// no entry for `path`: a refused add leaves the file readable and does not
/// persist the refused entry.
fn assert_repos_file_lacks_path(started: &StartedCoreNode, path: &str) {
    let repos_path = repositories_list_path(&started.peppy_dirs);
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("repos file must stay readable");
    assert!(
        repos.iter().all(|e| e["path"] != path),
        "refused entry must not be persisted: {repos:?}"
    );
}

/// An id at or above `u32::MAX` names no repository a cache entry can be
/// attributed to (and one above `i64::MAX` cannot even be read back), so
/// pinning one is a refusal naming the id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_explicit_id_out_of_range_fails() {
    let started = start_core_node_with_mock_messenger().await;

    for id in [u64::MAX, u64::from(u32::MAX)] {
        let resp = poll_core_node(
            &started,
            &RepoAddRequest::new_fs("/tmp/far-hub").with_id(id),
        )
        .await;
        assert!(!resp.success, "add pinning id {id} should fail");
        assert!(
            resp.error_message
                .contains(&format!("repository id {id} is out of range")),
            "error should name the id {id}, got: {}",
            resp.error_message
        );
        assert_repos_file_lacks_path(&started, "/tmp/far-hub");
    }
}

/// A derived id that reaches `u32::MAX` is refused as well, whether it
/// comes after the highest existing id or before the lowest one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_repo_add_derived_id_out_of_range_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let cases = [
        (u64::from(u32::MAX) - 1, false),
        (u64::from(u32::MAX) + 1, true),
    ];
    for (existing_id, top) in cases {
        write_repositories_json5(
            &started,
            &format!(r#"[{{ "id": {existing_id}, "type": "fs", "path": "/a" }}]"#),
        );

        let resp = poll_core_node(
            &started,
            &RepoAddRequest::new_fs("/tmp/far-hub").with_top(top),
        )
        .await;
        assert!(
            !resp.success,
            "add next to id {existing_id} (top={top}) should fail"
        );
        assert!(
            resp.error_message
                .contains(&format!("assigned id {} is out of range", u32::MAX)),
            "error should name the derived id, got: {}",
            resp.error_message
        );
        assert_repos_file_lacks_path(&started, "/tmp/far-hub");
    }
}
