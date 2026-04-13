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
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0" },
            execution: { language: "rust", run_cmd: ["sleep", "1"] }
        }"#,
    )
    .expect("write peppy.json5");

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
