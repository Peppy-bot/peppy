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
            top: false,
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
            top: false,
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
fn repo_add_fs_path_succeeds() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: work_dir
                .path()
                .join("my-local-repo")
                .to_str()
                .unwrap()
                .to_string(),
            git_ref: None,
            top: false,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo add fs path should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_add_url_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: "https://example.com/packages".to_string(),
            git_ref: None,
            top: false,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo add URL should succeed: {:?}",
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
            top: false,
        },
    }
    .execute(&ctx)
    .expect("first repo add should succeed");

    // Second add of same URL should fail
    let result = RepoCommand {
        command: RepoCommands::Add {
            source,
            git_ref: None,
            top: false,
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
fn repo_add_fs_path_with_ref_fails() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: work_dir
                .path()
                .join("my-local-repo")
                .to_str()
                .unwrap()
                .to_string(),
            git_ref: Some("main".to_string()),
            top: false,
        },
    }
    .execute(&ctx);

    let err = result.expect_err("fs path with --ref should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("--ref"),
        "error should mention --ref, got: {msg}"
    );
}

#[test]
fn repo_add_top_assigns_id_below_current_min() {
    let (_rt, serve, ctx, _work_dir) = setup();

    // First add — empty file, lands at the default floor (1000).
    RepoCommand {
        command: RepoCommands::Add {
            source: "https://example.com/first".to_string(),
            git_ref: None,
            top: false,
        },
    }
    .execute(&ctx)
    .expect("first add should succeed");

    // Second add with --top should land below the current min.
    RepoCommand {
        command: RepoCommands::Add {
            source: "https://example.com/second".to_string(),
            git_ref: None,
            top: true,
        },
    }
    .execute(&ctx)
    .expect("top add should succeed");

    let repos_path = serve.temp_dir().join("conf/repositories.json5");
    let content = std::fs::read_to_string(&repos_path).expect("read repos file");
    let repos: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse repos");

    let first = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/first")
        .expect("first entry missing");
    let second = repos
        .iter()
        .find(|e| e["url"] == "https://example.com/second")
        .expect("second (top) entry missing");

    assert_eq!(first["id"], 1000, "first add lands at the 1000 floor");
    assert_eq!(
        second["id"], 999,
        "--top should assign min(existing)-1 so the repo outranks all others"
    );
}

#[test]
fn repo_add_https_url_with_ref_treated_as_git() {
    // When --ref is provided, a parseable HTTPS URL (without `.git` suffix)
    // should be treated as a git clone URL rather than rejected.
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Add {
            source: "https://github.com/org/repo".to_string(),
            git_ref: Some("main".to_string()),
            top: false,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "https URL with --ref should succeed (treated as git): {:?}",
        result.err()
    );
}
