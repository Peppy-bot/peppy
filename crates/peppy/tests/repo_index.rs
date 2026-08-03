use daemon_config::consts::REPOSITORY_INDEX_FILE;
use peppy::commands::repo::repo_index;
use std::path::Path;
use tempfile::TempDir;

/// Helper: write a minimal valid node manifest under `dir`.
fn write_node(dir: &Path, name: &str, tag: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("peppy.json5"),
        format!(
            r#"{{
  peppy_schema: "node/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}},
  execution: {{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }},
}}"#
        ),
    )
    .unwrap();
}

/// Helper: write a minimal valid contract manifest at `path`.
fn write_contract(path: &Path, name: &str, tag: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            r#"{{
  peppy_schema: "contract/v1",
  manifest: {{ name: "{name}", tag: "{tag}" }},
  interfaces: {{}}
}}"#
        ),
    )
    .unwrap();
}

#[test]
fn repo_index_writes_the_file() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/uvc_camera"), "uvc_camera", "v1");
    write_contract(&repo.join("interfaces/rgb.json5"), "rgb_camera", "v1");

    repo_index(Some(repo.clone()), false).expect("indexing should succeed");

    let content = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();
    assert!(
        content.contains(r#"peppy_schema: "repository/v1""#),
        "{content}"
    );
    assert!(
        content.contains("nodes/uvc_camera/peppy.json5"),
        "{content}"
    );
    assert!(content.contains("interfaces/rgb.json5"), "{content}");
}

#[test]
fn repo_index_check_passes_on_a_generated_index() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");

    repo_index(Some(repo.clone()), false).expect("indexing should succeed");
    repo_index(Some(repo), true).expect("the index it just wrote should check out");
}

/// Writing twice produces the same file, so re-running generation after an
/// unrelated change never shows up as a spurious diff in a pull request.
#[test]
fn repo_index_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");
    write_node(&repo.join("nodes/b"), "b", "v1");

    repo_index(Some(repo.clone()), false).expect("first run");
    let first = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();
    repo_index(Some(repo.clone()), false).expect("second run");
    let second = std::fs::read_to_string(repo.join(REPOSITORY_INDEX_FILE)).unwrap();

    assert_eq!(first, second);
}

/// The mistake people will actually make: add an item, forget the index.
/// The check names the file and the identity, and says how to fix it.
#[test]
fn repo_index_check_names_the_unlisted_item() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");
    repo_index(Some(repo.clone()), false).expect("indexing should succeed");
    write_contract(&repo.join("robot/wrist.json5"), "wrist_link", "v1");

    let err = repo_index(Some(repo), true).expect_err("an unlisted item must fail the check");

    let message = err.to_string();
    assert!(message.contains("robot/wrist.json5"), "{message}");
    assert!(message.contains("wrist_link:v1"), "{message}");
    assert!(message.contains("peppy repo index"), "{message}");
}

/// A repository with no index does not check out, and the message names
/// the file that was looked for.
#[test]
fn repo_index_check_fails_when_the_index_is_missing() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("nodes/a"), "a", "v1");

    let err = repo_index(Some(repo), true).expect_err("a missing index must fail the check");
    assert!(err.to_string().contains(REPOSITORY_INDEX_FILE), "{err}");
}

/// Generation refuses a repository that claims one identity twice, on the
/// branch of the person who claimed it, naming both files.
#[test]
fn repo_index_refuses_an_identity_claimed_twice() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("hub");
    write_node(&repo.join("uvc/rust"), "uvc_camera", "v1");
    write_node(&repo.join("uvc/python"), "uvc_camera", "v1");

    let err =
        repo_index(Some(repo.clone()), false).expect_err("a contested identity must be refused");

    let message = err.to_string();
    assert!(message.contains("uvc_camera:v1"), "{message}");
    assert!(message.contains("uvc/rust/peppy.json5"), "{message}");
    assert!(message.contains("uvc/python/peppy.json5"), "{message}");
    assert!(
        !repo.join(REPOSITORY_INDEX_FILE).exists(),
        "no index is written when the repository contradicts itself"
    );
}

#[test]
fn repo_index_rejects_a_path_that_is_not_a_directory() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("not_a_dir");
    std::fs::write(&file, "").unwrap();

    let err = repo_index(Some(file), false).expect_err("a file is not a repository");
    assert!(err.to_string().contains("not a directory"), "{err}");
}
