/// Pinned ruff release tag used when building from source.
const RUFF_VERSION: &str = "0.15.0";

fn get_temp_cache_dir(cache_suffix: &str) -> std::path::PathBuf {
    let temp_dir = std::env::temp_dir();
    let cache_dir = temp_dir.join(format!("{}-peppy-cache", cache_suffix));

    if !cache_dir.exists() {
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    }

    cache_dir
}

mod ruff_build {
    use std::env;
    use std::process::Command;

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");

        // Use version-tagged temp directory for persistent cache
        let cache_dir = super::get_temp_cache_dir(&format!("ruff-{}", super::RUFF_VERSION));
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

mod peppylib_build {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");
        let so_path = peppylib_py_dir.join("peppylib/_peppylib.abi3.so");

        // Rerun when peppylib-py Rust source or Cargo.toml changes
        println!("cargo:rerun-if-changed=../peppylib-py/src/");
        println!("cargo:rerun-if-changed=../peppylib-py/Cargo.toml");

        // Use a separate CARGO_TARGET_DIR so maturin's inner `cargo build`
        // does not deadlock on the workspace build lock held by the outer cargo.
        let cache_dir = super::get_temp_cache_dir("peppylib-py");
        let target_dir = cache_dir.join("target");

        println!("cargo:warning=Building peppylib-py native extension via pixi…");

        let output = Command::new("pixi")
            .args(["run", "dev"])
            .current_dir(&peppylib_py_dir)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .output()
            .expect("Failed to run `pixi run dev` for peppylib-py");

        if !output.status.success() {
            panic!(
                "pixi run dev failed for peppylib-py:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        assert!(
            so_path.exists(),
            "Expected _peppylib.abi3.so at {:?} after pixi run dev, but not found",
            so_path,
        );

        // Emit an env var that changes when the .so is rebuilt. This forces
        // cargo to recompile the generator crate so rust_embed re-embeds the
        // fresh native extension.
        let mtime = std::fs::metadata(&so_path)
            .and_then(|m| m.modified())
            .expect("failed to read .so metadata")
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        println!("cargo:rustc-env=PEPPYLIB_SO_MTIME={mtime}");
    }
}

fn main() {
    ruff_build::run();
    peppylib_build::run();
}
