use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embed the git hash into the binary at compile time.
    // The serve command will write this to ~/.peppy/daemon_state.json5 at runtime.
    embed_git_hash();

    // Embed the git tag if provided (set by build_release.sh)
    build_helpers_shared::embed_git_tag();
}

fn embed_git_hash() {
    // Resolve the per-worktree git dir and the shared common git dir.
    // For submodules and linked worktrees, `<worktree>/.git` is a file
    // (gitlink), so naively joining `.git/HEAD` under the worktree root
    // points at a non-existent path — cargo would treat that missing
    // rerun-if-changed entry as stale and recompile on every build.
    let git_dir = run_git(&["rev-parse", "--absolute-git-dir"]);
    let git_common_dir = run_git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);

    let git_hash = run_git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PEPPY_GIT_HASH={}", git_hash);

    if let Some(dir) = git_dir {
        let head = PathBuf::from(&dir).join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }
    // The reflog is updated on every commit, merge, rebase, or checkout.
    // Unlike loose ref files under refs/heads/, the reflog is never
    // removed by `git pack-refs`, avoiding spurious rebuilds. It lives in
    // the common git dir, which is shared across linked worktrees.
    if let Some(common) = git_common_dir {
        let reflog = PathBuf::from(common).join("logs/HEAD");
        if reflog.exists() {
            println!("cargo:rerun-if-changed={}", reflog.display());
        }
    }
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
