use super::common::{setup, write_indexable_node};
use core_node::nodes_repo_cache_path;
use daemon_config::consts::PeppyDirs;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};

#[test]
fn repo_list_succeeds_with_defaults() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::List,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo list should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_list_finds_nodes_in_fs_repo() {
    let (_rt, serve, ctx, _work_dir) = setup();

    let peppy_dirs = PeppyDirs::new(serve.temp_dir());

    // Create a repo directory with a node
    let repo_dir = serve.temp_dir().join("test_repo");
    write_indexable_node(&repo_dir.join("my_sensor_v1"), "my_sensor", "v1");

    // Publish the repository and refresh it, so the node is discovered and
    // persisted to the cache, then list.
    super::common::seed_docs_repo(&serve, &ctx, &repo_dir);

    let result = RepoCommand {
        command: RepoCommands::List,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo list should succeed: {:?}",
        result.err()
    );

    // Verify discovery by inspecting the cache that refresh wrote.
    let cache_path = nodes_repo_cache_path(&peppy_dirs);
    let cache_content =
        std::fs::read_to_string(&cache_path).expect("nodes.json5 should be written by refresh");
    let cached: Vec<serde_json::Value> =
        serde_json5::from_str(&cache_content).expect("nodes.json5 should parse as JSON5");
    assert!(
        cached.iter().any(|n| {
            n.get("node_name").and_then(|v| v.as_str()) == Some("my_sensor")
                && n.get("node_tag").and_then(|v| v.as_str()) == Some("v1")
        }),
        "cache should contain my_sensor with node_tag v1, got: {cache_content}"
    );
}

#[test]
fn repo_list_empty_repos_succeeds() {
    let (_rt, serve, ctx, _work_dir) = setup();

    super::common::write_fs_repositories(&serve, &[]);

    let result = RepoCommand {
        command: RepoCommands::List,
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo list with empty repos should succeed: {:?}",
        result.err()
    );
}
