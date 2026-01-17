use std::fs;
use std::process::Command;

fn main() {
    // Re-run build script only when git HEAD changes (optimization to avoid unnecessary rebuilds)
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Capture git hash at build time for version tracking in releases.
    // The hash is embedded as GIT_HASH env var and written to git.hash file
    // for inclusion in release archives.
    // Uses "unknown" as fallback for non-git builds (e.g., release tarballs).
    let git_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);

    fs::write("git.hash", &git_hash).expect("Failed to write git.hash file");
}
