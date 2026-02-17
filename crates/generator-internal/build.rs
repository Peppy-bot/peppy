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

        // Set environment variable for runtime to find the ruff binary
        println!("cargo:rustc-env=RUFF_BINARY_PATH={}", ruff_binary_path);
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

        let profile = std::env::var("PROFILE").unwrap();
        let pixi_task = if profile == "release" {
            "release"
        } else {
            "dev"
        };

        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");

        // Serialize concurrent pixi invocations to avoid "Text file busy" races
        // when multiple build scripts run pixi on the same environment.
        let lock_path = peppylib_py_dir.join(".pixi/.build.lock");
        let _pixi_lock = acquire_pixi_lock(&lock_path);

        let output = Command::new("pixi")
            .args(["run", "-e", "default", pixi_task])
            .current_dir(&peppylib_py_dir)
            .env("CARGO_TARGET_DIR", &target_dir)
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

mod precompile_deps {
    use sha2::{Digest, Sha256};
    use std::env;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// Source crate directories to hash for cache invalidation (relative to workspace root).
    const SOURCE_CRATE_DIRS: &[&str] = &[
        "crates/peppylib",
        "crates/pmi-internal",
        "crates/config-internal",
    ];

    /// File extensions relevant to compilation.
    const RELEVANT_EXTENSIONS: &[&str] = &["rs", "toml", "capnp"];

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir
            .parent()
            .expect("generator crate must be inside workspace");
        let profile = env::var("PROFILE").unwrap();
        let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

        // Emit rerun-if-changed for all relevant source files
        let source_files = collect_source_files(workspace_root);
        for path in &source_files {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        // Rerun when workspace lock file changes (transitive dep version updates)
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join("Cargo.lock").display()
        );

        let cache_key = compute_cache_key(workspace_root, &source_files, &profile);
        let cache_dir = super::get_temp_cache_dir("precompiled-deps");
        let cached_dir = cache_dir.join(&cache_key);
        let precompiled_dir = out_dir.join("precompiled_rust");

        let manifest_entries = if cached_dir.join(".complete").exists() {
            println!(
                "cargo:warning=Using cached precompiled deps from {:?}",
                cached_dir
            );
            mirror_flat_dir(&cached_dir, &precompiled_dir);
            read_manifest_entries(&precompiled_dir)
        } else {
            println!("cargo:warning=Precompiling peppylib and dependencies…");
            let target_dir = cache_dir.join("build-target");
            let entries =
                build_and_collect(workspace_root, &target_dir, &profile, &precompiled_dir);

            // Populate cache
            mirror_flat_dir(&precompiled_dir, &cached_dir);
            fs::write(cached_dir.join(".complete"), "").unwrap();
            entries
        };

        generate_embed_file(&manifest_entries, &out_dir);
        println!("cargo:rustc-env=PEPPY_PRECOMPILED_CACHE_KEY={cache_key}");
    }

    /// Walk SOURCE_CRATE_DIRS and return all files with relevant extensions, sorted.
    fn collect_source_files(workspace_root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for dir_name in SOURCE_CRATE_DIRS {
            walk_relevant_files(&workspace_root.join(dir_name), &mut files);
        }
        files.sort();
        files
    }

    fn walk_relevant_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_string_lossy();
                if name == "target" || name == "tests" || name == "examples" {
                    continue;
                }
                walk_relevant_files(&path, out);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && RELEVANT_EXTENSIONS.contains(&ext)
            {
                out.push(path);
            }
        }
    }

    fn compute_cache_key(workspace_root: &Path, source_files: &[PathBuf], profile: &str) -> String {
        let mut hasher = Sha256::new();

        // Hash source file contents (using paths relative to workspace for portability)
        for path in source_files {
            let rel = path.strip_prefix(workspace_root).unwrap_or(path);
            hasher.update(rel.to_string_lossy().as_bytes());
            if let Ok(contents) = fs::read(path) {
                hasher.update(&contents);
            }
        }

        // Hash Cargo.lock for transitive dependency pinning
        if let Ok(lock) = fs::read(workspace_root.join("Cargo.lock")) {
            hasher.update(&lock);
        }

        // Hash rustc version for ABI compatibility
        if let Ok(output) = Command::new("rustc")
            .args(["--version", "--verbose"])
            .output()
        {
            hasher.update(&output.stdout);
        }

        hasher.update(profile.as_bytes());
        // Include the precompile RUSTFLAGS so changing optimization invalidates cache
        hasher.update(b"-Cdebuginfo=0");
        // Include the host triple (used as --target) so cross-target changes invalidate
        let host_triple = env::var("HOST").unwrap_or_default();
        hasher.update(host_triple.as_bytes());

        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{profile}", &hex[..16])
    }

    fn build_and_collect(
        workspace_root: &Path,
        target_dir: &Path,
        profile: &str,
        precompiled_dir: &Path,
    ) -> Vec<(String, String, String)> {
        if precompiled_dir.exists() {
            fs::remove_dir_all(precompiled_dir).unwrap();
        }
        fs::create_dir_all(precompiled_dir).unwrap();

        // Use `--target $HOST` to force cargo to place target artifacts in a
        // separate `<target_dir>/<triple>/` directory, cleanly separating them
        // from host-compiled artifacts (proc-macro dependencies). This prevents
        // collecting duplicate rlibs for crates that appear in both the target
        // and proc-macro host dependency graphs (e.g. schemars).
        let host_triple = env::var("HOST").expect("HOST env var not set by cargo");

        let mut cmd = Command::new("cargo");
        cmd.current_dir(workspace_root)
            .env("CARGO_TARGET_DIR", target_dir)
            // Strip debug info to keep embedded artifacts small.
            // Type metadata is preserved; only DWARF symbols are dropped.
            // Use CARGO_ENCODED_RUSTFLAGS so it overrides the outer cargo's flags.
            .env("CARGO_ENCODED_RUSTFLAGS", "-Cdebuginfo=0")
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .args([
                "build",
                "-p",
                "peppylib",
                "--lib",
                "--message-format=json",
                "--target",
                &host_triple,
            ]);

        if profile == "release" {
            cmd.arg("--release");
        }

        let mut child = cmd
            .spawn()
            .expect("failed to start cargo build for precompilation");
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);

        // Target artifacts live under `<target_dir>/<triple>/`; host artifacts
        // (proc-macro deps) live directly under `<target_dir>/`.
        // We discriminate using the path prefix.
        let target_prefix = target_dir.join(&host_triple);
        let target_prefix_str = target_prefix.to_string_lossy().to_string();

        // (crate_name, filename, kind)
        let mut manifest_entries: Vec<(String, String, String)> = Vec::new();

        for line in reader.lines() {
            let line = line.expect("failed to read cargo output");
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };

            if json.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
                continue;
            }

            let Some(target) = json.get("target") else {
                continue;
            };
            let Some(crate_name) = target.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let crate_types = target
                .get("crate_types")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let is_proc_macro = crate_types.iter().any(|t| t.as_str() == Some("proc-macro"));
            let kind = if is_proc_macro { "proc-macro" } else { "lib" };

            let Some(filenames) = json.get("filenames").and_then(|v| v.as_array()) else {
                continue;
            };

            for f in filenames {
                let Some(path_str) = f.as_str() else {
                    continue;
                };
                let path = PathBuf::from(path_str);
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !matches!(ext, "rlib" | "dylib" | "so") {
                    continue;
                }

                // Keep target rlibs (under <target_dir>/<triple>/) and proc-macro
                // dylibs (always host-compiled). Skip host-only rlibs to avoid
                // duplicate artifacts for the same crate name.
                let is_target_artifact = path_str.starts_with(&target_prefix_str);
                if !is_target_artifact && !is_proc_macro {
                    continue;
                }

                let file_name = path.file_name().unwrap().to_string_lossy().to_string();
                fs::copy(&path, precompiled_dir.join(&file_name))
                    .unwrap_or_else(|e| panic!("failed to copy {path_str}: {e}"));

                manifest_entries.push((crate_name.to_string(), file_name, kind.to_string()));
            }
        }

        let status = child.wait().expect("cargo build did not exit");
        assert!(status.success(), "cargo build for precompilation failed");

        // Write manifest to precompiled_dir so it persists in the cache
        write_manifest_entries(&manifest_entries, precompiled_dir);
        manifest_entries
    }

    /// Generate a Rust file that embeds all precompiled artifacts via `include_bytes!`.
    fn generate_embed_file(manifest_entries: &[(String, String, String)], out_dir: &Path) {
        let mut code = String::new();

        // Manifest as a Rust array: (crate_name, filename, kind)
        code.push_str("static PRECOMPILED_MANIFEST: &[(&str, &str, &str)] = &[\n");
        for (crate_name, filename, kind) in manifest_entries {
            code.push_str(&format!(
                "    (\"{crate_name}\", \"{filename}\", \"{kind}\"),\n"
            ));
        }
        code.push_str("];\n\n");

        // Artifact bytes
        code.push_str("static PRECOMPILED_ARTIFACTS: &[(&str, &[u8])] = &[\n");
        for (_crate_name, filename, _kind) in manifest_entries {
            code.push_str(&format!(
                "    (\"{filename}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/precompiled_rust/{filename}\"))),\n",
            ));
        }
        code.push_str("];\n");

        fs::write(out_dir.join("precompiled_rust_embed.rs"), code).unwrap();
    }

    /// Write manifest entries as a tab-separated text file.
    fn write_manifest_entries(entries: &[(String, String, String)], dir: &Path) {
        let mut content = String::new();
        for (crate_name, filename, kind) in entries {
            content.push_str(&format!("{crate_name}\t{filename}\t{kind}\n"));
        }
        fs::write(dir.join("manifest.txt"), content).unwrap();
    }

    /// Read manifest entries from a tab-separated text file.
    fn read_manifest_entries(dir: &Path) -> Vec<(String, String, String)> {
        let content = fs::read_to_string(dir.join("manifest.txt"))
            .expect("failed to read manifest.txt from cached precompiled dir");
        content
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                assert_eq!(parts.len(), 3, "malformed manifest line: {line}");
                (
                    parts[0].to_string(),
                    parts[1].to_string(),
                    parts[2].to_string(),
                )
            })
            .collect()
    }

    /// Copy all regular files (not subdirs, not `.complete` markers) from `src` to `dst`.
    fn mirror_flat_dir(src: &Path, dst: &Path) {
        if dst.exists() {
            fs::remove_dir_all(dst).ok();
        }
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap().flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().unwrap();
                if name != ".complete" {
                    fs::copy(&path, dst.join(name)).unwrap();
                }
            }
        }
    }
}

fn main() {
    ruff_build::run();
    peppylib_build::run();
    precompile_deps::run();
}
