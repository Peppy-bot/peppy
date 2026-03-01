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
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Returns the platform suffix for the current host (e.g. "macos-aarch64", "linux-x86_64").
    fn host_platform_suffix() -> String {
        let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
        format!("{os}-{arch}")
    }

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

    /// Runs a pixi task and panics on failure.
    fn run_pixi_task(peppylib_py_dir: &Path, task: &str, target_dir: &Path) {
        let output = Command::new("pixi")
            .args(["run", "-e", "default", task])
            .current_dir(peppylib_py_dir)
            .env("CARGO_TARGET_DIR", target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .output()
            .unwrap_or_else(|e| panic!("Failed to run `pixi run {task}`: {e}"));

        if !output.status.success() {
            panic!(
                "pixi run {task} failed for peppylib-py:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    /// Extracts `_peppylib.abi3.so` from the newest `.whl` file in the given directory.
    ///
    /// Maturin wheels are zip archives containing the `.so` at `peppylib/_peppylib.abi3.so`.
    fn extract_so_from_wheel(wheels_dir: &Path) -> Vec<u8> {
        let whl_path = std::fs::read_dir(wheels_dir)
            .expect("failed to read wheels directory")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "whl"))
            .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
            .map(|e| e.path())
            .expect("no .whl file found in wheels directory");

        let file = File::open(&whl_path).expect("failed to open wheel file");
        let mut archive = zip::ZipArchive::new(file).expect("failed to read wheel as zip archive");

        let so_entry_name = archive
            .file_names()
            .find(|name| name.ends_with("_peppylib.abi3.so"))
            .expect("wheel does not contain _peppylib.abi3.so")
            .to_string();

        let mut entry = archive
            .by_name(&so_entry_name)
            .expect("failed to read .so entry from wheel");

        let mut buf = Vec::with_capacity(entry.size() as usize);
        std::io::Read::read_to_end(&mut entry, &mut buf).expect("failed to extract .so from wheel");
        buf
    }

    /// Ensures the `aarch64-unknown-linux-gnu` Rust target is installed.
    fn ensure_linux_rust_target() {
        let status = Command::new("rustup")
            .args(["target", "add", "aarch64-unknown-linux-gnu"])
            .status()
            .expect("failed to run rustup target add");
        if !status.success() {
            panic!("rustup target add aarch64-unknown-linux-gnu failed");
        }
    }

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");
        let peppylib_dir = peppylib_py_dir.join("peppylib");
        let so_path = peppylib_dir.join("_peppylib.abi3.so");

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

        // Serialize concurrent pixi invocations to avoid "Text file busy" races
        // when multiple build scripts run pixi on the same environment.
        let lock_path = peppylib_py_dir.join(".pixi/.build.lock");
        let _pixi_lock = acquire_pixi_lock(&lock_path);

        // 1. Build the native .so (host platform)
        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");
        run_pixi_task(&peppylib_py_dir, pixi_task, &target_dir);

        assert!(
            so_path.exists(),
            "Expected _peppylib.abi3.so at {:?} after pixi run {pixi_task}, but not found",
            so_path,
        );

        let host_suffix = host_platform_suffix();

        // 2. Rename native .so to platform-suffixed name
        let native_so_path = peppylib_dir.join(format!("_peppylib.abi3.{host_suffix}.so"));
        std::fs::rename(&so_path, &native_so_path).unwrap_or_else(|e| {
            panic!(
                "failed to rename {:?} to {:?}: {e}",
                so_path, native_so_path
            )
        });

        // 3. On macOS: cross-compile the Linux .so via maturin + zig
        #[cfg(target_os = "macos")]
        {
            let cross_pixi_task = if profile == "release" {
                "cross-linux-release"
            } else {
                "cross-linux-dev"
            };

            println!(
                "cargo:warning=Cross-compiling peppylib-py for linux-aarch64 via pixi ({cross_pixi_task})…"
            );

            ensure_linux_rust_target();
            run_pixi_task(&peppylib_py_dir, cross_pixi_task, &target_dir);

            // The cross-compiled wheel is written to {target_dir}/wheels/
            let wheels_dir = target_dir.join("wheels");
            let linux_so_bytes = extract_so_from_wheel(&wheels_dir);

            let linux_so_path = peppylib_dir.join("_peppylib.abi3.linux-aarch64.so");
            std::fs::write(&linux_so_path, &linux_so_bytes).unwrap_or_else(|e| {
                panic!("failed to write linux .so to {:?}: {e}", linux_so_path)
            });

            // Clean up wheel files to avoid stale artifacts
            std::fs::remove_dir_all(&wheels_dir).ok();
        }

        // Remove the original unsuffixed .so if it still exists (shouldn't after rename,
        // but guard against partial rebuilds)
        if so_path.exists() {
            std::fs::remove_file(&so_path).ok();
        }

        // 4. Compute a combined hash of all platform .so files for cache invalidation
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let mut so_files: Vec<_> = std::fs::read_dir(&peppylib_dir)
            .expect("failed to read peppylib directory")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("_peppylib.abi3.") && n.ends_with(".so"))
            })
            .map(|e| e.path())
            .collect();
        so_files.sort();
        for so_file in &so_files {
            let bytes = std::fs::read(so_file)
                .unwrap_or_else(|e| panic!("failed to read {:?} for hashing: {e}", so_file));
            hasher.update(so_file.file_name().unwrap().as_encoded_bytes());
            hasher.update(&bytes);
        }
        let hash = hasher.finalize();
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
