use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use std::sync::Arc;

fn setup() -> (
    tokio::runtime::Runtime,
    ServeCommandEmulation,
    Arc<AppContext>,
    tempfile::TempDir,
) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let work_dir = tempfile::tempdir().expect("failed to create temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    (rt, serve, ctx, work_dir)
}

/// Minimal valid peppy.json5 content for a node with the given name and tag.
fn minimal_peppy_json5(name: &str, tag: &str) -> String {
    format!(
        r#"{{
  schema_version: 1,
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
    let node_dir = repo_dir.join("my_sensor_1.0.0");
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        minimal_peppy_json5("my_sensor", "1.0.0"),
    )
    .expect("write peppy.json5");

    // Write repositories.json5 pointing to that directory
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).expect("create conf dir");
    std::fs::write(
        conf_dir.join("repositories.json5"),
        format!(r#"[{{ "type": "fs", "path": "{}" }}]"#, repo_dir.display()),
    )
    .expect("write repos file");

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
