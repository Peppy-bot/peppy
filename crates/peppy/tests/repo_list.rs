use super::common::setup;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use core_node::nodes_repo_cache_path;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};

/// Minimal valid peppy.json5 content for a node with the given name and tag.
fn minimal_peppy_json5(name: &str, tag: &str) -> String {
    format!(
        r#"{{
  peppy_schema: "node_v1",
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
    let node_dir = repo_dir.join("my_sensor_v1");
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        minimal_peppy_json5("my_sensor", "v1"),
    )
    .expect("write peppy.json5");

    // Write repositories.json5 pointing to that directory
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(
        conf_dir.join("repositories.json5"),
        format!(
            r#"[{{ "id": 1, "type": "fs", "path": "{}" }}]"#,
            repo_dir.display()
        ),
    )
    .expect("write repos file");

    // Refresh so the node is discovered and persisted to the cache, then list.
    RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx)
    .expect("repo refresh should succeed");

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
        "cache should contain my_sensor/1.0.0, got: {cache_content}"
    );
}

#[test]
fn repo_list_empty_repos_succeeds() {
    let (_rt, serve, ctx, _work_dir) = setup();

    let peppy_dirs = PeppyDirs::new(serve.temp_dir());

    // Write an empty repositories.json5
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(conf_dir.join("repositories.json5"), "[]").expect("write repos file");

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
