use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embed the git hash into the binary at compile time.
    // The serve command will write this to ~/.peppy/daemon_state.json5 at runtime.
    embed_git_hash();

    // Embed the git tag if provided (set by build_release.sh)
    build_helpers::embed_git_tag();
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
        // The reflog is updated on every commit, merge, rebase, or checkout.
        // Unlike loose ref files under refs/heads/, the reflog is never
        // removed by `git pack-refs`, avoiding spurious rebuilds.
        let reflog = git_dir.join("logs/HEAD");
        if reflog.exists() {
            println!("cargo:rerun-if-changed={}", reflog.display());
        }
    }
}
