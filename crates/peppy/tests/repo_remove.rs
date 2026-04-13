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
fn repo_remove_after_add_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    // First add a repo so there's something to remove
    RepoCommand {
        command: RepoCommands::Add {
            source: "/tmp/my-local-repo".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("repo add should succeed");

    // Remove by id (first added repo gets id=1)
    let result = RepoCommand {
        command: RepoCommands::Remove { id: 1 },
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

    // Add a git repo
    RepoCommand {
        command: RepoCommands::Add {
            source: "https://github.com/org/repo.git".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("repo add git should succeed");

    // Remove it by id
    let result = RepoCommand {
        command: RepoCommands::Remove { id: 1 },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo remove git should succeed: {:?}",
        result.err()
    );
}
