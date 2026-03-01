/// Pinned ruff release tag used when building from source.
const RUFF_VERSION: &str = "0.15.0";

/// Recursively collect all files under `dir`.
fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "cargo:warning=Failed to read directory {}: {}",
                    current.display(),
                    e
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn get_temp_cache_dir(cache_suffix: &str) -> std::path::PathBuf {
    let user_home = std::env::var("HOME").expect("HOME environment variable not set");
    let cache_dir = std::path::PathBuf::from(user_home)
        .join(".peppy/tmp")
        .join(cache_suffix);

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
        println!("cargo:rustc-env=RUFF_VERSION={}", super::RUFF_VERSION);

        let profile = env::var("PROFILE").unwrap();
        let is_release = profile == "release";

        // Use version-tagged temp directory for persistent cache
        let cache_dir = super::get_temp_cache_dir(&format!("ruff-{}", super::RUFF_VERSION));
        let cached_ruff_path = cache_dir.join(format!("ruff-{profile}"));

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
            let mut cmd = Command::new("cargo");
            cmd.current_dir(&build_dir)
                .env("RUSTUP_TOOLCHAIN", "stable")
                .env_remove("RUSTC")
                .env_remove("RUSTDOC");
            if is_release {
                cmd.args(["build", "--release", "--bin", "ruff"]);
            } else {
                cmd.args(["build", "--bin", "ruff"]);
            }
            let status = cmd.status();

            if status.is_err() || !status.unwrap().success() {
                panic!("Failed to build ruff binary");
            }

            // Copy to cache with version tag
            let target_subdir = if is_release { "release" } else { "debug" };
            std::fs::copy(
                build_dir.join(format!("target/{target_subdir}/ruff")),
                &cached_ruff_path,
            )
            .expect("Failed to cache ruff binary");

            // Copy to OUT_DIR for runtime
            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("Failed to copy ruff binary to OUT_DIR");

            // Clean up build directory
            std::fs::remove_dir_all(&build_dir).ok();
        }
    }
}

mod peppylib_build {
    use std::fs::File;
    use std::path::PathBuf;
    use std::process::Command;

    fn acquire_pixi_lock(lock_path: &std::path::Path) -> File {
        let lock_dir = lock_path
            .parent()
            .expect("pixi lock path should include a parent directory");
        std::fs::create_dir_all(lock_dir).expect("Failed to create pixi lock directory");

        let lock_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .expect("Failed to open pixi lock file");

        lock_file.lock().expect("Failed to acquire pixi build lock");
        lock_file
    }

    fn platform_tag() -> String {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "linux" => "linux",
            os => panic!("Unsupported OS: {os}"),
        };
        format!("{os}_{}", std::env::consts::ARCH)
    }

    fn compile_so(
        peppylib_py_dir: &std::path::Path,
        target_dir: &std::path::Path,
        pixi_task: &str,
    ) -> PathBuf {
        let so_path = peppylib_py_dir.join("peppylib/_peppylib.abi3.so");

        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");

        // Serialize concurrent pixi invocations to avoid "Text file busy" races
        // when multiple build scripts run pixi on the same environment.
        let lock_path = peppylib_py_dir.join(".pixi/.build.lock");
        let _pixi_lock = acquire_pixi_lock(&lock_path);

        let output = Command::new("pixi")
            .args(["run", "-e", "default", pixi_task])
            .current_dir(peppylib_py_dir)
            .env("CARGO_TARGET_DIR", target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .output()
            .expect("Failed to run `pixi run` for peppylib-py");

        if !output.status.success() {
            panic!(
                "pixi run {pixi_task} failed for peppylib-py:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        assert!(
            so_path.exists(),
            "Expected _peppylib.abi3.so at {:?} after pixi run {pixi_task}, but not found",
            so_path,
        );

        // Move .so into platform-specific subdirectory so rust_embed embeds it
        // under the correct platform tag (e.g. peppylib/macos_aarch64/).
        let tag = platform_tag();
        let platform_dir = peppylib_py_dir.join(format!("peppylib/{tag}"));
        std::fs::create_dir_all(&platform_dir)
            .expect("Failed to create platform subdirectory for .so");
        let final_so_path = platform_dir.join("_peppylib.abi3.so");
        std::fs::rename(&so_path, &final_so_path)
            .expect("Failed to move .so to platform subdirectory");

        // Make the platform dir a proper Python package so importlib.import_module works.
        let init_path = platform_dir.join("__init__.py");
        if !init_path.exists() {
            std::fs::write(&init_path, "").expect("Failed to create __init__.py");
        }

        final_so_path
    }

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");

        // Rerun when peppylib-py Rust source or Cargo.toml changes
        println!("cargo:rerun-if-changed=../peppylib-py/Cargo.toml");
        let src_dir = peppylib_py_dir.join("src");
        if src_dir.is_dir() {
            for entry in super::walkdir(&src_dir) {
                println!("cargo:rerun-if-changed={}", entry.display());
            }
        }

        // Use a separate CARGO_TARGET_DIR so maturin's inner `cargo build`
        // does not deadlock on the workspace build lock held by the outer cargo.
        let cache_dir = super::get_temp_cache_dir("peppylib-py");
        let target_dir = cache_dir.join("target");

        let profile = std::env::var("PROFILE").unwrap();
        let pixi_task = if profile == "release" {
            "release"
        } else {
            "dev"
        };

        let so_path = compile_so(&peppylib_py_dir, &target_dir, pixi_task);

        // Emit a content hash that changes when the .so is rebuilt. This forces
        // cargo to recompile the generator crate so rust_embed re-embeds the
        // fresh native extension.
        use sha2::{Digest, Sha256};
        let so_bytes = std::fs::read(&so_path).expect("failed to read .so for hashing");
        let hash = Sha256::digest(&so_bytes);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        println!("cargo:rustc-env=PEPPYLIB_SO_HASH={}", &hex[..16]);
    }
}

/// Generates `embedded_ruff.rs` in OUT_DIR that embeds the ruff binary via `include_bytes!`.
/// This allows the binary to be extracted at runtime on any machine, rather than relying
/// on a stale compile-time filesystem path.
fn embed_ruff_binary() {
    use std::io::Write;

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ruff_binary_path = out_dir.join("ruff");
    let generated = out_dir.join("embedded_ruff.rs");

    let mut file = std::fs::File::create(&generated).unwrap();
    if ruff_binary_path.exists() {
        writeln!(
            file,
            r#"pub const RUFF_BINARY: Option<&[u8]> = Some(include_bytes!("{}"));"#,
            ruff_binary_path.display()
        )
        .unwrap();
    } else {
        writeln!(file, r#"pub const RUFF_BINARY: Option<&[u8]> = None;"#).unwrap();
    }
}

mod rust_crates_build {
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    /// Returns true if the file should be included, matching the rust_embed attributes:
    /// include: *.rs, *.toml, *.capnp, *.j2, tools/capnp_*
    /// exclude: target/*, tests/*, examples/*
    fn should_include(relative_path: &str, is_config_internal: bool) -> bool {
        // Exclude patterns
        if relative_path.starts_with("target/")
            || relative_path.starts_with("tests/")
            || relative_path.starts_with("examples/")
        {
            return false;
        }

        // Include patterns
        if relative_path.ends_with(".rs")
            || relative_path.ends_with(".toml")
            || relative_path.ends_with(".capnp")
            || relative_path.ends_with(".j2")
        {
            return true;
        }

        // config-internal has tools/capnp_* include pattern
        if is_config_internal && relative_path.starts_with("tools/capnp_") {
            return true;
        }

        false
    }

    fn collect_files(dir: &std::path::Path, is_config_internal: bool) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];

        while let Some(current) = stack.pop() {
            let entries = match std::fs::read_dir(&current) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!(
                        "cargo:warning=Failed to read directory {}: {}",
                        current.display(),
                        e
                    );
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let rel = path.strip_prefix(dir).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy();
                    // Skip excluded directories entirely
                    if rel_str == "target" || rel_str == "tests" || rel_str == "examples" {
                        continue;
                    }
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(dir).unwrap_or(&path);
                    let rel_str = rel.to_string_lossy();
                    if should_include(&rel_str, is_config_internal) {
                        files.push(path);
                    }
                }
            }
        }

        files
    }

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let crate_dirs = [
            ("../peppylib", false),
            ("../pmi-internal", false),
            ("../config-internal", true),
        ];

        let mut hasher = Sha256::new();

        for (rel, is_config) in &crate_dirs {
            let dir = manifest_dir.join(rel);
            let mut files = collect_files(&dir, *is_config);
            // Sort for deterministic hashing
            files.sort();

            for file_path in &files {
                println!("cargo:rerun-if-changed={}", file_path.display());
                let relative = file_path.strip_prefix(&dir).unwrap_or(file_path);
                hasher.update(relative.to_string_lossy().as_bytes());
                match std::fs::read(file_path) {
                    Ok(content) => hasher.update(&content),
                    Err(e) => eprintln!(
                        "cargo:warning=Failed to read file for hashing {}: {}",
                        file_path.display(),
                        e
                    ),
                }
            }
        }

        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        println!("cargo:rustc-env=RUST_CRATES_HASH={}", &hex[..16]);
    }
}

fn main() {
    ruff_build::run();
    embed_ruff_binary();
    peppylib_build::run();
    rust_crates_build::run();
}
