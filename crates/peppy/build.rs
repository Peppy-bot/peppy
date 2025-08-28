use std::env;
use std::path::PathBuf;
use std::process::Command;

// Version tags for external binaries (should match Cargo.toml dependencies where applicable)
const PIXI_VERSION: &str = "v0.52.0";
const ZENOH_VERSION: &str = "1.5.0";

fn get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
    let temp_dir = env::temp_dir();
    let cache_dir = temp_dir.join(format!("{}-peppy-cache", cache_suffix));

    // Create cache directory if it doesn't exist
    if !cache_dir.exists() {
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    }

    cache_dir
}

fn build_pixi(release_tag: &str) {
    // Build pixi binary when the build_pixi feature is enabled
    if env::var("CARGO_FEATURE_BUILD_PIXI").is_ok() {
        println!("cargo:rerun-if-changed=build.rs");

        // Use named temp directory for persistent cache
        let cache_dir = get_temp_cache_dir("pixi");
        let cached_pixi_path = cache_dir.join(format!("pixi-{}", release_tag));

        // Always copy to OUT_DIR for runtime access
        let out_dir = env::var("OUT_DIR").unwrap();
        let pixi_binary_path = format!("{}/pixi", out_dir);

        // Check if pixi is already cached
        if cached_pixi_path.exists() {
            println!(
                "cargo:warning=Using cached pixi binary from {:?}",
                cached_pixi_path
            );

            // Copy cached binary to OUT_DIR
            std::fs::copy(&cached_pixi_path, &pixi_binary_path)
                .expect("Failed to copy cached pixi binary");
        } else {
            println!("cargo:warning=Building pixi binary from source...");

            // Build in a temporary directory within cache
            let build_dir = cache_dir.join("pixi-src");
            if build_dir.exists() {
                let _ = std::fs::remove_dir_all(&build_dir);
            }

            // Clone pixi repository
            let output = Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    release_tag,
                    "https://github.com/prefix-dev/pixi",
                    build_dir.to_str().unwrap(),
                ])
                .output()
                .expect("Failed to execute git clone");

            if !output.status.success() {
                println!(
                    "cargo:warning=Failed to clone pixi repository: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }

            // Build pixi
            let status = Command::new("cargo")
                .current_dir(&build_dir)
                .args(["build", "--release", "--bin", "pixi"])
                .status();

            if status.is_err() || !status.unwrap().success() {
                println!("cargo:warning=Failed to build pixi binary");
                return;
            }

            // Copy to cache with version tag
            std::fs::copy(build_dir.join("target/release/pixi"), &cached_pixi_path)
                .expect("Failed to cache pixi binary");

            // Copy to OUT_DIR for runtime
            std::fs::copy(&cached_pixi_path, &pixi_binary_path)
                .expect("Failed to copy pixi binary to OUT_DIR");

            // Clean up build directory
            let _ = std::fs::remove_dir_all(&build_dir);
        }

        // Set environment variable for runtime to find the pixi binary
        println!("cargo:rustc-env=PIXI_BINARY_PATH={}", pixi_binary_path);
    }
}

fn build_zenoh(release_tag: &str) {
    // Build zenoh router binary when the build_zenoh feature is enabled
    if env::var("CARGO_FEATURE_BUILD_ZENOH").is_ok() {
        println!("cargo:rerun-if-changed=build.rs");

        // Use named temp directory for persistent cache
        let cache_dir = get_temp_cache_dir("zenoh");
        let cached_zenoh_path = cache_dir.join(format!("zenohd-{}", release_tag));

        // Always copy to OUT_DIR for runtime access
        let out_dir = env::var("OUT_DIR").unwrap();
        let zenoh_binary_path = format!("{}/zenohd", out_dir);

        // Check if zenohd is already cached
        if cached_zenoh_path.exists() {
            println!(
                "cargo:warning=Using cached zenohd binary from {:?}",
                cached_zenoh_path
            );

            // Copy cached binary to OUT_DIR
            std::fs::copy(&cached_zenoh_path, &zenoh_binary_path)
                .expect("Failed to copy cached zenohd binary");
        } else {
            println!("cargo:warning=Building zenohd binary from source...");

            // Build in a temporary directory within cache
            let build_dir = cache_dir.join("zenoh-src");
            if build_dir.exists() {
                let _ = std::fs::remove_dir_all(&build_dir);
            }

            // Clone zenoh repository
            let output = Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    release_tag,
                    "https://github.com/eclipse-zenoh/zenoh",
                    build_dir.to_str().unwrap(),
                ])
                .output()
                .expect("Failed to execute git clone");

            if !output.status.success() {
                println!(
                    "cargo:warning=Failed to clone zenoh repository: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }

            // Build zenohd
            let status = Command::new("cargo")
                .current_dir(&build_dir)
                .args(["build", "--release", "--bin", "zenohd"])
                .status();

            if status.is_err() || !status.unwrap().success() {
                println!("cargo:warning=Failed to build zenohd binary");
                return;
            }

            // Copy to cache with version tag
            std::fs::copy(build_dir.join("target/release/zenohd"), &cached_zenoh_path)
                .expect("Failed to cache zenohd binary");

            // Copy to OUT_DIR for runtime
            std::fs::copy(&cached_zenoh_path, &zenoh_binary_path)
                .expect("Failed to copy zenohd binary to OUT_DIR");

            // Clean up build directory
            let _ = std::fs::remove_dir_all(&build_dir);
        }

        // Set environment variable for runtime to find the zenohd binary
        println!("cargo:rustc-env=ZENOHD_BINARY_PATH={}", zenoh_binary_path);
    }
}

fn main() {
    build_pixi(PIXI_VERSION);
    build_zenoh(ZENOH_VERSION);
}
