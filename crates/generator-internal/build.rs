/// Pinned ruff release tag used when building from source.
const RUFF_VERSION: &str = "0.15.0";

mod ruff_build {
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    fn get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
        let temp_dir = env::temp_dir();
        let cache_dir = temp_dir.join(format!("{}-peppy-cache", cache_suffix));

        // Create cache directory if it doesn't exist
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
        }

        cache_dir
    }

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");

        // Use version-tagged temp directory for persistent cache
        let cache_dir = get_temp_cache_dir(&format!("ruff-{}", super::RUFF_VERSION));
        let cached_ruff_path = cache_dir.join("ruff");

        // Always copy to OUT_DIR for runtime access
        let out_dir = env::var("OUT_DIR").unwrap();
        let ruff_binary_path = format!("{}/ruff", out_dir);

        // Check if ruff is already cached
        if cached_ruff_path.exists() {
            println!(
                "cargo:warning=Using cached ruff binary from {:?}",
                cached_ruff_path
            );

            // Copy cached binary to OUT_DIR
            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("Failed to copy cached ruff binary");
        } else {
            println!("cargo:warning=Building ruff binary from source...");

            // Build in a temporary directory within cache
            let build_dir = cache_dir.join("ruff-src");
            if build_dir.exists() {
                std::fs::remove_dir_all(&build_dir).ok();
            }

            // Clone ruff repository
            let output = Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    super::RUFF_VERSION,
                    "https://github.com/astral-sh/ruff",
                    build_dir.to_str().unwrap(),
                ])
                .output()
                .expect("Failed to execute git clone");

            if !output.status.success() {
                panic!(
                    "Failed to clone ruff repository: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            // Build ruff using the stable toolchain, which may be newer than the
            // project's default toolchain pinned via rustup. We must also clear
            // RUSTC/RUSTDOC because Cargo injects the current toolchain's paths
            // into the build-script environment, which would override the
            // RUSTUP_TOOLCHAIN selection.
            let status = Command::new("cargo")
                .current_dir(&build_dir)
                .env("RUSTUP_TOOLCHAIN", "stable")
                .env_remove("RUSTC")
                .env_remove("RUSTDOC")
                .args(["build", "--release", "--bin", "ruff"])
                .status();

            if status.is_err() || !status.unwrap().success() {
                panic!("Failed to build ruff binary");
            }

            // Copy to cache with version tag
            std::fs::copy(build_dir.join("target/release/ruff"), &cached_ruff_path)
                .expect("Failed to cache ruff binary");

            // Copy to OUT_DIR for runtime
            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("Failed to copy ruff binary to OUT_DIR");

            // Clean up build directory
            std::fs::remove_dir_all(&build_dir).ok();
        }

        // Set environment variable for runtime to find the ruff binary
        println!("cargo:rustc-env=RUFF_BINARY_PATH={}", ruff_binary_path);
    }
}

fn main() {
    ruff_build::run();
}
