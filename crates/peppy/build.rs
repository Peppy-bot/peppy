use std::env;
use std::process::Command;

fn build_pixi(release_tag: &str) {
    // Build pixi binary when the build_pixi feature is enabled
    if env::var("CARGO_FEATURE_BUILD_PIXI").is_ok() {
        println!("cargo:rerun-if-changed=build.rs");

        // Clone and build pixi as a separate binary
        let out_dir = env::var("OUT_DIR").unwrap();
        let pixi_binary_path = format!("{}/pixi", out_dir);

        // Check if pixi needs to be built
        if !std::path::Path::new(&pixi_binary_path).exists() {
            println!("cargo:warning=Building pixi binary from source...");

            // Clean up any existing pixi-src directory first
            let pixi_src_dir = format!("{}/pixi-src", out_dir);
            if std::path::Path::new(&pixi_src_dir).exists() {
                let _ = std::fs::remove_dir_all(&pixi_src_dir);
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
                    &pixi_src_dir,
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
                .current_dir(format!("{}/pixi-src", out_dir))
                .args(["build", "--release", "--bin", "pixi"])
                .status();

            if status.is_err() || !status.unwrap().success() {
                println!("cargo:warning=Failed to build pixi binary");
                return;
            }

            // Copy the built binary to our output directory
            let _ = std::fs::copy(
                format!("{}/pixi-src/target/release/pixi", out_dir),
                &pixi_binary_path,
            );
        }

        // Set environment variable for runtime to find the pixi binary
        println!("cargo:rustc-env=PIXI_BINARY_PATH={}", pixi_binary_path);
    }
}

fn main() {
    build_pixi("v0.52.0");
}
