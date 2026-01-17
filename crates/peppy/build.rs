use std::env;
use std::path::PathBuf;
use std::process::Command;

fn _get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
    let temp_dir = env::temp_dir();
    let cache_dir = temp_dir.join(format!("{}-peppy-cache", cache_suffix));

    // Create cache directory if it doesn't exist
    if !cache_dir.exists() {
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    }

    cache_dir
}

fn main() {
    // Generate git.hash file next to the binary for release fingerprint tracking.
    // This allows development builds to work with the fingerprint verification system.
    generate_git_hash();
}

fn generate_git_hash() {
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

    // Write git.hash to the peppy data directory (~/.peppy/)
    // This matches the logic in config::consts::peppy_data_dir()
    let peppy_data_dir = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".peppy");

    if std::fs::create_dir_all(&peppy_data_dir).is_ok() {
        let git_hash_path = peppy_data_dir.join("git.hash");
        if std::fs::write(&git_hash_path, format!("{}\n", git_hash)).is_err() {
            // Silently ignore write errors (e.g., permission issues)
            // The fingerprint check will fail later with a clear error message
        }
    }

    // Tell cargo to rerun this if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/");
}
