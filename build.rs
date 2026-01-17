use std::fs;
use std::process::Command;

fn main() {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to get git hash");

    let git_hash = String::from_utf8(output.stdout).unwrap();
    let git_hash = git_hash.trim();

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);

    // Write git hash to file for inclusion in release archives
    fs::write("git.hash", git_hash).expect("Failed to write git.hash file");
}
