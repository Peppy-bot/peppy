use super::common::setup;
use core_node_api::encoding::GitRepoRef;
use peppy::commands::Command;
use peppy::commands::repo::{RepoCommand, RepoCommands};
use peppy::context::AppContext;
use std::sync::Arc;

/// `repo exclude SOURCE [--ref REF]`.
fn exclude_repo(
    ctx: &Arc<AppContext>,
    source: &str,
    git_ref: Option<&str>,
) -> peppy::error::Result<()> {
    RepoCommand {
        command: RepoCommands::Exclude {
            source: source.to_string(),
            git_ref: git_ref.map(|name| GitRepoRef::Named(name.to_string())),
        },
    }
    .execute(ctx)
}

#[test]
fn repo_exclude_git_url_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = exclude_repo(&ctx, "https://github.com/org/repo.git", None);

    assert!(
        result.is_ok(),
        "repo exclude should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_with_git_ref_succeeds() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = exclude_repo(&ctx, "https://github.com/org/repo.git", Some("v1.0.0"));

    assert!(
        result.is_ok(),
        "repo exclude with ref should succeed: {:?}",
        result.err()
    );
}

#[test]
fn repo_exclude_fs_path_succeeds() {
    let (_rt, _serve, ctx, work_dir) = setup();

    let result = exclude_repo(
        &ctx,
        work_dir.path().join("my-local-repo").to_str().unwrap(),
        None,
    );

    assert!(
        result.is_ok(),
        "repo exclude fs path should succeed: {:?}",
        result.err()
    );
}

/// Exclusion classifies its source the same way `repo add` does, so a URL
/// that names no repository is refused here too rather than being written
/// as an exclusion that can never match anything.
#[test]
fn repo_exclude_refuses_a_url_that_is_not_a_git_clone_url() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let result = exclude_repo(&ctx, "https://example.com/packages", None);

    let msg = result
        .expect_err("a bare http URL is not a repository")
        .to_string();
    assert!(msg.contains("git URL"), "error should name git, got: {msg}");
    assert!(
        msg.contains("directory on this machine"),
        "error should name fs, got: {msg}"
    );
}

#[test]
fn repo_exclude_duplicate_fails() {
    let (_rt, _serve, ctx, _work_dir) = setup();

    let source = "https://github.com/org/repo.git";

    // First exclude should succeed
    exclude_repo(&ctx, source, None).expect("first repo exclude should succeed");

    // Second exclude of same URL should fail
    let result = exclude_repo(&ctx, source, None);

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

    let result = exclude_repo(
        &ctx,
        work_dir.path().join("my-local-repo").to_str().unwrap(),
        Some("main"),
    );

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

    let result = exclude_repo(&ctx, "https://github.com/org/repo", Some("main"));

    assert!(
        result.is_ok(),
        "https URL with --ref should succeed (treated as git): {:?}",
        result.err()
    );
}
