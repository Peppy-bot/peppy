use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use std::sync::Arc;

/// Read repositories.json5 and find the id of the entry whose "path" or "url"
/// field matches `source`.  Panics if no match is found.
fn find_repo_id(serve: &ServeCommandEmulation, source: &str) -> u32 {
    let repos_path = serve.temp_dir().join("conf/repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("failed to read repositories.json5");
    let repos: Vec<serde_json::Value> =
        serde_json::from_str(&content).expect("failed to parse repositories.json5");
    for entry in &repos {
        let matches = entry
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| p == source)
            .unwrap_or(false)
            || entry
                .get("url")
                .and_then(|v| v.as_str())
                .map(|u| u == source)
                .unwrap_or(false);
        if matches {
            return entry
                .get("id")
                .and_then(|v| v.as_u64())
                .expect("repo entry missing id") as u32;
        }
    }
    panic!("no repo entry found matching source '{source}'");
}

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
fn repo_remove_after_add_succeeds() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let source_path = work_dir.path().join("my-local-repo");
    let source = source_path.to_str().unwrap();

    // First add a repo so there's something to remove
    RepoCommand {
        command: RepoCommands::Add {
            source: source.to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("repo add should succeed");

    // Find the actual id assigned to the repo we just added
    let id = find_repo_id(&_serve, source);
    let result = RepoCommand {
        command: RepoCommands::Remove { id },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo remove should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_remove_nonexistent_id_fails() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Remove { id: 999 },
    }
    .execute(&ctx);

    let err = result.expect_err("repo remove of nonexistent id should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("not found"),
        "error should mention 'not found', got: {msg}"
    );
}

#[test]
fn repo_remove_after_add_git_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let source = "https://github.com/org/repo.git";

    // Add a git repo
    RepoCommand {
        command: RepoCommands::Add {
            source: source.to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("repo add git should succeed");

    // Find the actual id assigned to the repo we just added
    let id = find_repo_id(&_serve, source);
    let result = RepoCommand {
        command: RepoCommands::Remove { id },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo remove git should succeed: {:?}",
        result.err()
    );
}
