mod common;

use common::{
    StartedCoreNode, excluded_repositories_path, poll_core_node, read_json5_array,
    start_core_node_with_mock_messenger, write_excluded_repositories_json5,
};
use config::failure_kind::PeppyFailureKind;
use core_node_api::encoding::{GitRepoRef, RepoExcludeRequest};

/// Overwrites the default entries the daemon wrote at start with an empty
/// `repositories.json5`. The exclude handler re-indexes every repository
/// before it responds, and the defaults are git hubs that the re-index would
/// clone over the network.
fn write_empty_repositories_json5(started: &StartedCoreNode) {
    common::write_repositories_json5(started, "[]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_git_without_ref_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    poll_core_node(
        &started,
        &RepoExcludeRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead),
    )
    .await;

    // Verify the file was created with the new entry
    let repos = read_json5_array(&excluded_repositories_path(&started.peppy_dirs));

    assert_eq!(repos.len(), 1);
    let entry = &repos[0];
    assert_eq!(entry["type"], "git");
    assert_eq!(entry["url"], "https://example.com/packages.git");
    assert!(
        entry.get("ref").is_none_or(serde_json::Value::is_null),
        "an exclusion without a ref matches the repository at any ref"
    );
    assert_eq!(entry["id"], 1, "first entry should get id 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclude_git_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    write_empty_repositories_json5(&started);

    poll_core_node(
        &started,
        &RepoExcludeRequest::new_git(
            "https://github.com/example/repo.git",
            GitRepoRef::Named("main".to_string()),
        ),
    )
    .await;

    let repos = read_json5_array(&excluded_repositories_path(&started.peppy_dirs));

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

    poll_core_node(&started, &RepoExcludeRequest::new_fs("/tmp/my-local-repo")).await;

    let repos = read_json5_array(&excluded_repositories_path(&started.peppy_dirs));

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

    let request =
        RepoExcludeRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead);

    // First exclude should succeed
    poll_core_node(&started, &request).await;

    // Second exclude of the same URL should fail
    let failure = common::poll_core_node_failure(&started, &request).await;
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);
    assert!(
        failure.message().contains("already exists"),
        "error should mention 'already exists', got: {}",
        failure.message()
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

    let failure = common::poll_core_node_failure(
        &started,
        &RepoExcludeRequest::new_git("https://example.com/new.git", GitRepoRef::RemoteHead),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::AttemptFailed);
    assert!(
        failure.message().contains("duplicate repository id"),
        "error should mention duplicate id, got: {}",
        failure.message()
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

    poll_core_node(
        &started,
        &RepoExcludeRequest::new_git("https://example.com/packages.git", GitRepoRef::RemoteHead),
    )
    .await;

    let repos = read_json5_array(&excluded_repositories_path(&started.peppy_dirs));

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
