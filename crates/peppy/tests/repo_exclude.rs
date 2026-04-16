use super::common::setup;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};

#[test]
fn repo_exclude_git_url_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: "https://github.com/org/repo.git".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo exclude should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_with_git_ref_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: "https://github.com/org/repo.git".to_string(),
            git_ref: Some("v1.0.0".to_string()),
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo exclude with ref should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_fs_path_succeeds() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: work_dir
                .path()
                .join("my-local-repo")
                .to_str()
                .unwrap()
                .to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo exclude fs path should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_url_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: "https://example.com/packages".to_string(),
            git_ref: None,
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "repo exclude URL should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_duplicate_fails() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let source = "https://github.com/org/repo.git".to_string();

    // First exclude should succeed
    RepoCommand {
        command: RepoCommands::Exclude {
            source: source.clone(),
            git_ref: None,
        },
    }
    .execute(&ctx)
    .expect("first repo exclude should succeed");

    // Second exclude of same URL should fail
    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source,
            git_ref: None,
        },
    }
    .execute(&ctx);

    let err = result.expect_err("duplicate repo exclude should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("already exists"),
        "error should mention 'already exists', got: {msg}"
    );
}

#[test]
fn repo_exclude_fs_path_with_ref_fails() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: work_dir
                .path()
                .join("my-local-repo")
                .to_str()
                .unwrap()
                .to_string(),
            git_ref: Some("main".to_string()),
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
fn repo_exclude_https_url_with_ref_treated_as_git() {
    // When --ref is provided, a parseable HTTPS URL (without `.git` suffix)
    // is treated as a git clone URL rather than rejected, matching the
    // behavior of `repo add`.
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = RepoCommand {
        command: RepoCommands::Exclude {
            source: "https://github.com/org/repo".to_string(),
            git_ref: Some("main".to_string()),
        },
    }
    .execute(&ctx);

    assert!(
        result.is_ok(),
        "https URL with --ref should succeed (treated as git): {:?}",
        result.err()
    );
}
