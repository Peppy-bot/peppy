mod common;

use common::{CALLER_INSTANCE_ID, StartedCoreNode, start_core_node_with_mock_messenger};
use config::consts::NODE_CONFIG_FILE;
use core_node::{nodes_repo_cache_path, repositories_list_path};
use core_node_api::encoding::{
    RepoListRepoFailureKind, RepoListRequest, RepoListResponse, RepoSourceKind,
};
use peppylib::core_node::transport::poll;
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

/// Write a nodes.json5 cache file in the cache_dir of the started core node.
fn write_packages_cache(started: &StartedCoreNode, content: &str) {
    let cache_dir = started.peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir).expect("create cache dir");
    std::fs::write(nodes_repo_cache_path(&started.peppy_dirs), content).expect("write cache file");
}

/// One `nodes.json5` entry for an fs-discovered node, shaped exactly as
/// `repo refresh` records it: the path points at the manifest file.
///
/// `repo list` reads the caches for every source kind rather than
/// re-walking fs repositories, so an fs node has to be indexed to be
/// listed, just like a git one.
fn fs_cache_entry(node_dir: &std::path::Path, name: &str, tag: &str) -> serde_json::Value {
    serde_json::json!({
        "node_name": name,
        "node_tag": tag,
        "source_type": "fs",
        "path": node_dir.join(NODE_CONFIG_FILE).to_string_lossy(),
    })
}

/// Write `entries` as the nodes cache.
fn index_nodes(started: &StartedCoreNode, entries: Vec<serde_json::Value>) {
    write_packages_cache(
        started,
        &serde_json::to_string(&serde_json::Value::Array(entries)).unwrap(),
    );
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
    poll(
        &RepoListRequest,
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("repo_list poll should succeed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_default_repos_creates_repositories_file() {
    let started = start_core_node_with_mock_messenger().await;

    let repos_path = repositories_list_path(&started.peppy_dirs);

    let resp = send_repo_list(&started).await;
    assert!(resp.success, "repo_list should succeed");
    assert!(resp.error_message.is_none());

    assert!(
        repos_path.exists(),
        "repositories.json5 should be created with default entries on core node startup"
    );

    let content = std::fs::read_to_string(&repos_path).expect("read created file");
    let repos: Vec<serde_json::Value> =
        serde_json5::from_str(&content).expect("parse created file");
    // Don't pin the count; the shipped defaults grow over time. Just
    // verify the canonical nodes-hub entry is there.
    let nodes_hub = repos
        .iter()
        .find(|r| {
            r.get("url").and_then(|v| v.as_str())
                == Some("https://github.com/Peppy-bot/nodes-hub.git")
        })
        .expect("default repos should include nodes-hub");
    assert_eq!(nodes_hub.get("type").unwrap().as_str().unwrap(), "git");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_finds_nodes_in_fs_repo() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("test_repo");
    let sensor_dir = create_node_dir(&repo_dir, "my_sensor", "v1");
    let actuator_dir = create_node_dir(&repo_dir, "my_actuator", "v2");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() }
        ]))
        .unwrap(),
    );
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&sensor_dir, "my_sensor", "v1"),
            fs_cache_entry(&actuator_dir, "my_actuator", "v2"),
        ],
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

    // Write a nodes.json5 cache with nodes from that git repo
    write_packages_cache(
        &started,
        &serde_json::to_string(&serde_json::json!([
            {
                "node_name": "git_sensor",
                "node_tag": "v1",
                "source_type": "git",
                "source_uri": git_url,
                "resolved_ref": "main",
                "path": "nodes/git_sensor"
            },
            {
                "node_name": "git_actuator",
                "node_tag": "v2",
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
    assert_eq!(sensor.node_tag, "v1");
    assert_eq!(sensor.source_type, RepoSourceKind::Git);
    assert_eq!(sensor.path, "nodes/git_sensor");
    assert_eq!(sensor.repo_id, 1);
    assert_eq!(sensor.repo_label, expected_label);

    let actuator = resp
        .nodes
        .iter()
        .find(|n| n.node_name == "git_actuator")
        .expect("should find git_actuator");
    assert_eq!(actuator.node_tag, "v2");
    assert_eq!(actuator.source_type, RepoSourceKind::Git);
    assert_eq!(actuator.path, "nodes/git_actuator");
    assert_eq!(actuator.repo_id, 1);
    assert_eq!(actuator.repo_label, expected_label);
}

/// When two FS repositories provide the same node, the list should contain both
/// entries: the first as non-duplicate and the second marked as duplicate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_marks_cross_repo_duplicates_fs() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("list_dup_a");
    let repo_b = started.peppy_dirs.root().join("list_dup_b");
    let shared_a = create_node_dir(&repo_a, "shared", "v1");
    let shared_b = create_node_dir(&repo_b, "shared", "v1");
    let unique_b = create_node_dir(&repo_b, "unique_b", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_a.to_string_lossy() },
            { "id": 2, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&shared_a, "shared", "v1"),
            fs_cache_entry(&shared_b, "shared", "v1"),
            fs_cache_entry(&unique_b, "unique_b", "v1"),
        ],
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
    let overlapping = create_node_dir(&repo_dir, "overlapping", "v1");

    let git_url = "https://github.com/example/nodes.git";

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() },
            { "id": 2, "type": "git", "url": git_url }
        ]))
        .unwrap(),
    );

    // Both repositories provide the same identity; the lower id wins.
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&overlapping, "overlapping", "v1"),
            serde_json::json!({
                "node_name": "overlapping",
                "node_tag": "v1",
                "source_type": "git",
                "source_uri": git_url,
                "resolved_ref": "main",
                "path": "nodes/overlapping"
            }),
        ],
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
    // The label describes the repository as configured, which is what
    // `repo add` and `repo exclude` print, so a user can match them up.
    // This repository pins no ref, so the label is the bare url even
    // though the cached entry resolved to `main`.
    assert_eq!(git_entry.repo_label, git_url);
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
    let node_a = create_node_dir(&repo_a, "node_a", "v1");
    let node_b = create_node_dir(&repo_b, "node_b", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_a.to_string_lossy() },
            { "id": 2, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );
    // Both are indexed; the exclusion is what keeps repo_b out of the list.
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&node_a, "node_a", "v1"),
            fs_cache_entry(&node_b, "node_b", "v1"),
        ],
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

/// `repo list` reports what will actually resolve, so a node sitting on
/// disk but absent from the index is not listed: it cannot be launched
/// until the next refresh, and showing it would promise otherwise.
///
/// Subdirectory exclusion is applied while walking, so it is covered by
/// the refresh tests rather than here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_omits_nodes_that_are_on_disk_but_not_indexed() {
    let started = start_core_node_with_mock_messenger().await;

    let repo = started.peppy_dirs.root().join("mixed_repo");
    let indexed = create_node_dir(&repo, "keep_node", "v1");
    create_node_dir(&repo, "added_since_the_last_refresh", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo.to_string_lossy() }
        ]))
        .unwrap(),
    );
    index_nodes(&started, vec![fs_cache_entry(&indexed, "keep_node", "v1")]);

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(
        resp.nodes.len(),
        1,
        "only the indexed node should be listed"
    );
    assert_eq!(resp.nodes[0].node_name, "keep_node");
}

/// An excluded git repo should not appear in the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_excludes_git_repo() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_dir = started.peppy_dirs.root().join("fs_repo");
    let fs_node = create_node_dir(&repo_dir, "fs_node", "v1");

    let git_url = "https://github.com/example/excluded.git";

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_dir.to_string_lossy() },
            { "id": 2, "type": "git", "url": git_url }
        ]))
        .unwrap(),
    );
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&fs_node, "fs_node", "v1"),
            serde_json::json!({
                "node_name": "git_node",
                "node_tag": "v1",
                "source_type": "git",
                "source_uri": git_url,
                "path": "nodes/git_node"
            }),
        ],
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

/// Two repositories claiming one identity and one repository claiming it
/// twice are different situations and must be labelled differently.
/// Reporting a conflict as harmless shadowing is what let the original
/// defect hide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_distinguishes_conflict_from_cross_repo_shadowing() {
    let started = start_core_node_with_mock_messenger().await;

    let repo_a = started.peppy_dirs.root().join("repo_a");
    let repo_b = started.peppy_dirs.root().join("repo_b");
    let shadowed_a = create_node_dir(&repo_a, "shadowed", "v1");
    let shadowed_b = create_node_dir(&repo_b, "shadowed", "v1");
    let contested_one = create_node_dir(&repo_b, "contested_one", "v1");
    let contested_two = create_node_dir(&repo_b, "contested_two", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo_a.to_string_lossy() },
            { "id": 2, "type": "fs", "path": repo_b.to_string_lossy() }
        ]))
        .unwrap(),
    );
    // `contested` claimed twice inside repo_b: a cache an older peppy
    // could have written, since refresh now refuses to produce one.
    index_nodes(
        &started,
        vec![
            fs_cache_entry(&shadowed_a, "shadowed", "v1"),
            fs_cache_entry(&shadowed_b, "shadowed", "v1"),
            fs_cache_entry(&contested_one, "contested", "v1"),
            fs_cache_entry(&contested_two, "contested", "v1"),
        ],
    );

    let resp = send_repo_list(&started).await;
    assert!(resp.success);

    let shadowed: Vec<_> = resp
        .nodes
        .iter()
        .filter(|n| n.node_name == "shadowed")
        .collect();
    assert_eq!(shadowed.len(), 2);
    assert!(
        shadowed.iter().all(|n| !n.conflict),
        "shadowing across repositories is not a conflict"
    );
    assert_eq!(
        shadowed.iter().filter(|n| n.duplicate).count(),
        1,
        "exactly the losing entry is marked as shadowed"
    );

    let contested: Vec<_> = resp
        .nodes
        .iter()
        .filter(|n| n.node_name == "contested")
        .collect();
    assert_eq!(contested.len(), 2);
    assert!(
        contested.iter().all(|n| n.conflict),
        "both claimants inside one repository are marked as conflicting"
    );
}

/// A repository whose last refresh failed is reported as retained, with
/// the timestamp of the read its entries actually came from and a reason
/// that says whether it was an outage or a content bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_reports_retained_entries_and_the_failure_reason() {
    let started = start_core_node_with_mock_messenger().await;

    let repo = started.peppy_dirs.root().join("stale_repo");
    let node = create_node_dir(&repo, "kept", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo.to_string_lossy() }
        ]))
        .unwrap(),
    );
    index_nodes(&started, vec![fs_cache_entry(&node, "kept", "v1")]);
    std::fs::write(
        started.peppy_dirs.cache_dir().join("repo_status.json5"),
        serde_json::to_string(&serde_json::json!([{
            "id": 1,
            "identity": repo.to_string_lossy(),
            "source_type": "fs",
            "last_read_unix_secs": 1_753_900_000u64,
            "last_failure": {
                "kind": "unreachable",
                "message": "path does not exist",
                "unix_secs": 1_753_986_400u64
            }
        }]))
        .unwrap(),
    )
    .expect("write repo status");

    let resp = send_repo_list(&started).await;
    assert!(resp.success);

    assert_eq!(resp.repos.len(), 1);
    let repo_status = &resp.repos[0];
    assert_eq!(repo_status.id, 1);
    assert!(repo_status.retained, "its last read failed");
    assert_eq!(
        repo_status.last_read_unix_secs,
        Some(1_753_900_000),
        "the entries date from the last clean read, not from now"
    );
    let failure = repo_status.failure.as_ref().expect("reason reported");
    assert_eq!(
        failure.kind,
        RepoListRepoFailureKind::Unreachable,
        "an outage must not read as a content bug"
    );

    // The retained entry is still listed: launchers referencing it keep working.
    assert_eq!(resp.nodes.len(), 1);
    assert_eq!(resp.nodes[0].node_name, "kept");
}

/// A repository that read cleanly is current, with no failure and no
/// retained marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_reports_a_healthy_repository_as_current() {
    let started = start_core_node_with_mock_messenger().await;

    let repo = started.peppy_dirs.root().join("healthy_repo");
    let node = create_node_dir(&repo, "fresh", "v1");

    write_repositories_json5(
        &started,
        &serde_json::to_string(&serde_json::json!([
            { "id": 1, "type": "fs", "path": repo.to_string_lossy() }
        ]))
        .unwrap(),
    );
    index_nodes(&started, vec![fs_cache_entry(&node, "fresh", "v1")]);
    std::fs::write(
        started.peppy_dirs.cache_dir().join("repo_status.json5"),
        serde_json::to_string(&serde_json::json!([{
            "id": 1,
            "identity": repo.to_string_lossy(),
            "source_type": "fs",
            "last_read_unix_secs": 1_753_900_000u64
        }]))
        .unwrap(),
    )
    .expect("write repo status");

    let resp = send_repo_list(&started).await;
    assert!(resp.success);
    assert_eq!(resp.repos.len(), 1);
    assert!(!resp.repos[0].retained);
    assert!(resp.repos[0].failure.is_none());
    assert_eq!(resp.nodes.len(), 1);
    assert!(!resp.nodes[0].conflict);
    assert!(!resp.nodes[0].duplicate);
}
