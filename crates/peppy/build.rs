use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embed the git hash into the binary at compile time.
    // The serve command will write this to ~/.peppy/git.hash at runtime.
    embed_git_hash();
}

fn embed_git_hash() {
    // Get the git repository root directory
    let git_root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| PathBuf::from(s.trim()));

    // Get the git hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Embed the git hash into the binary via environment variable
    println!("cargo:rustc-env=PEPPY_GIT_HASH={}", git_hash);

    // Tell cargo to rerun this if git HEAD changes.
    // We must use absolute paths because cargo resolves relative paths from the
    // crate's Cargo.toml directory, not the workspace root where .git/ lives.
    if let Some(root) = git_root {
        let git_dir = root.join(".git");
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        // Also track the current branch ref file for when commits are made
        if let Ok(head_content) = std::fs::read_to_string(git_dir.join("HEAD")) {
            if let Some(ref_path) = head_content.trim().strip_prefix("ref: ") {
                println!(
                    "cargo:rerun-if-changed={}",
                    git_dir.join(ref_path).display()
                );
            }
        }
    }
}
