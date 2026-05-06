use config::consts::PeppyDirs;
use core_node::repositories_list_path;
use peppy::commands::repo::repo_init_with_dirs;
use serde_json::Value;
use tempfile::TempDir;

/// Helper: read repositories.json5 as a `Vec<Value>`. Both the seeded
/// template and the rewritten file are JSON5 (unquoted keys, optional
/// trailing commas), so use the JSON5 parser in both cases.
fn read_repos_json(peppy_dirs: &PeppyDirs) -> Vec<Value> {
    let path = repositories_list_path(peppy_dirs);
    let content = std::fs::read_to_string(&path).unwrap();
    serde_json5::from_str(&content).unwrap()
}

fn has_git_url(repos: &[Value], url: &str) -> bool {
    repos.iter().any(|e| {
        e.get("type").and_then(|v| v.as_str()) == Some("git")
            && e.get("url").and_then(|v| v.as_str()) == Some(url)
    })
}

#[test]
fn repo_init_creates_file_when_missing() {
    let tmp = TempDir::new().unwrap();
    let peppy_dirs = PeppyDirs::new(tmp.path());
    let repos_path = repositories_list_path(&peppy_dirs);
    assert!(!repos_path.exists());

    repo_init_with_dirs(&peppy_dirs).expect("repo init should succeed");

    assert!(repos_path.exists(), "repositories.json5 should be created");
    // Fresh-write path keeps the JSON5 template verbatim (comments and all),
    // so don't try to parse it as strict JSON — match on substring instead.
    let content = std::fs::read_to_string(&repos_path).unwrap();
    assert!(
        content.contains("Peppy-bot/launchers_hub.git"),
        "default launchers_hub entry should be present in template, got:\n{content}"
    );
}

/// Reproduces the user-reported bug from the daemon: existing
/// `repositories.json5` only contains an older default, and the user upgrades
/// peppy. `peppy repo init` must add the missing defaults.
#[test]
fn repo_init_appends_missing_defaults_to_existing_file() {
    let tmp = TempDir::new().unwrap();
    let peppy_dirs = PeppyDirs::new(tmp.path());
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).unwrap();
    std::fs::write(
        conf_dir.join("repositories.json5"),
        r#"[
            { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes_hub", "ref": "main" }
        ]"#,
    )
    .unwrap();

    repo_init_with_dirs(&peppy_dirs).expect("repo init should succeed");

    let repos = read_repos_json(&peppy_dirs);
    assert!(
        has_git_url(&repos, "https://github.com/Peppy-bot/nodes_hub"),
        "pre-existing nodes_hub entry must be preserved"
    );
    assert!(
        has_git_url(&repos, "https://github.com/Peppy-bot/launchers_hub.git"),
        "missing launchers_hub default must be appended"
    );
}

#[test]
fn repo_init_preserves_user_repos() {
    let tmp = TempDir::new().unwrap();
    let peppy_dirs = PeppyDirs::new(tmp.path());
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).unwrap();
    std::fs::write(
        conf_dir.join("repositories.json5"),
        r#"[
            { "id": 1, "type": "fs", "path": "/home/me/my_nodes" }
        ]"#,
    )
    .unwrap();

    repo_init_with_dirs(&peppy_dirs).expect("repo init should succeed");

    let repos = read_repos_json(&peppy_dirs);
    assert!(
        repos
            .iter()
            .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("fs")
                && e.get("path").and_then(|v| v.as_str()) == Some("/home/me/my_nodes")),
        "user fs repo must be preserved across init"
    );
}

#[test]
fn repo_init_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let peppy_dirs = PeppyDirs::new(tmp.path());
    let repos_path = repositories_list_path(&peppy_dirs);

    repo_init_with_dirs(&peppy_dirs).expect("first init");
    let first = std::fs::read_to_string(&repos_path).unwrap();

    repo_init_with_dirs(&peppy_dirs).expect("second init");
    let second = std::fs::read_to_string(&repos_path).unwrap();

    assert_eq!(first, second, "running init twice must not modify the file");
}

#[test]
fn repo_init_returns_error_on_corrupt_file() {
    let tmp = TempDir::new().unwrap();
    let peppy_dirs = PeppyDirs::new(tmp.path());
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir).unwrap();
    std::fs::write(
        conf_dir.join("repositories.json5"),
        "this is not valid json5 {{{",
    )
    .unwrap();

    let err = repo_init_with_dirs(&peppy_dirs).expect_err("corrupt file should be reported");
    let msg = err.to_string();
    assert!(
        msg.contains("repositories.json5"),
        "error should mention the file, got: {msg}"
    );
}
