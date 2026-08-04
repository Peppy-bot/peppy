use super::common::setup;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};

#[test]
fn repo_refresh_succeeds_with_default_repos() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo refresh should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_refresh_succeeds_after_adding_fs_repo() {
    let (_rt, serve, ctx, _work_dir) = setup();

    // Write a repositories.json5 with an fs entry pointing at a temp dir
    // that contains a valid peppy.json5.
    let node_dir = tempfile::tempdir().expect("temp node dir");
    std::fs::write(
        node_dir.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write peppy.json5");
    super::common::publish_repo_index(node_dir.path());

    // Seed the repos file in the daemon's conf dir
    let conf_dir = serve.temp_dir().join("conf");
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": node_dir.path().to_string_lossy() }
    ]))
    .expect("serialize repos");
    std::fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    let result = RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo refresh with fs repo should succeed: {:?}",
        result.err()
    );
}

/// A repository that cannot be read does not fail the command. The
/// installer runs `repo refresh` as its last step, so a hub that is
/// offline, or a repository root with no committed
/// `peppy_repository.json5`, would otherwise end an install that had
/// worked up to that point. The command reports what it could not read
/// and leaves the user with a retry.
#[test]
fn repo_refresh_succeeds_when_a_repository_cannot_be_read() {
    let (_rt, serve, ctx, _work_dir) = setup();

    let healthy = tempfile::tempdir().expect("temp node dir");
    std::fs::write(
        healthy.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "reachable_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write peppy.json5");
    super::common::publish_repo_index(healthy.path());

    // Never published an index, so the repository states nothing about
    // what it holds.
    let unpublished = tempfile::tempdir().expect("temp unpublished repo");
    std::fs::write(
        unpublished.path().join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "unpublished_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write peppy.json5");

    let conf_dir = serve.temp_dir().join("conf");
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": healthy.path().to_string_lossy() },
        { "id": 2, "type": "fs", "path": unpublished.path().to_string_lossy() },
        { "id": 3, "type": "fs", "path": serve.temp_dir().join("never_mounted").to_string_lossy() },
    ]))
    .expect("serialize repos");
    std::fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    let result = RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "a repository that cannot be read must not fail the command: {:?}",
        result.err()
    );

    // The repositories that did read are current: containment is only
    // worth anything if the rest of the run still lands.
    let peppy_dirs = daemon_config::consts::PeppyDirs::new(serve.temp_dir());
    let cache = std::fs::read_to_string(core_node::nodes_repo_cache_path(&peppy_dirs))
        .expect("the node cache should be published");
    assert!(
        cache.contains("reachable_node"),
        "the readable repository still updated:\n{cache}"
    );
}

/// A `pairing/v1` document in an fs repo is discovered by `repo refresh`
/// and lands in the daemon's pairing cache file.
#[test]
fn repo_refresh_discovers_pairing_docs() {
    let (_rt, serve, ctx, _work_dir) = setup();

    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    let peppy_dirs = daemon_config::consts::PeppyDirs::new(serve.temp_dir());
    let cache_path = core_node::pairings_repo_cache_path(&peppy_dirs);
    assert!(
        cache_path.exists(),
        "pairing cache should exist at {}",
        cache_path.display()
    );
    let cache = std::fs::read_to_string(&cache_path).expect("pairing cache should read");
    assert!(
        cache.contains("arm_link"),
        "pairing cache should contain the discovered doc:\n{cache}"
    );
}
