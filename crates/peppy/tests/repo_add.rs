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
fn repo_add_git_url_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: "https://github.com/org/repo.git".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo add should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_add_with_git_ref_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: "https://github.com/org/repo.git".to_string(),
            git_ref: Some("v1.0.0".to_string()),
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo add with ref should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_add_duplicate_fails() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let source = "https://github.com/org/repo.git".to_string();

    // First add should succeed
    RepoCommand {
        command: RepoCommands::Add {
            source: source.clone(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("first repo add should succeed");

    // Second add of same URL should fail
    let result = RepoCommand {
        command: RepoCommands::Add {
            source,
            git_ref: None,
        },
    }
    .execute(&ctx);

    let err = result.expect_err("duplicate repo add should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("already exists"),
        "error should mention 'already exists', got: {msg}"
    );
}

#[test]
fn repo_add_non_git_url_fails() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: "https://example.com/archive.tar.gz".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx);

    let err = result.expect_err("non-git URL should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("not supported"),
        "error should mention URL repos not supported, got: {msg}"
    );
}
