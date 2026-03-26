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
                println!(
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

mod ruff_build {
    use std::env;
    use std::process::Command;

    /// Try to download a pre-built ruff binary from GitHub releases.
    fn download_ruff(target: &str, dest: &std::path::Path) -> bool {
        let url = format!(
            "https://github.com/astral-sh/ruff/releases/download/{version}/ruff-{target}.tar.gz",
            version = super::RUFF_VERSION,
        );

        println!(
            "cargo:warning=Downloading ruff {} for {}...",
            super::RUFF_VERSION,
            target
        );

        let temp_dir = dest.parent().unwrap().join("ruff-download-tmp");
        std::fs::create_dir_all(&temp_dir).ok();

        let status = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "curl -sSfL '{}' | tar xz --strip-components=1 -C '{}'",
                    url,
                    temp_dir.display()
                ),
            ])
            .status();

        if status.is_err() || !status.as_ref().unwrap().success() {
            println!(
                "cargo:warning=Failed to download ruff for {} (no pre-built binary available)",
                target
            );
            std::fs::remove_dir_all(&temp_dir).ok();
            return false;
        }

        let ruff_bin = temp_dir.join("ruff");
        if ruff_bin.exists() {
            if let Err(e) = std::fs::copy(&ruff_bin, dest) {
                println!("cargo:warning=Failed to copy downloaded ruff binary: {e}");
                std::fs::remove_dir_all(&temp_dir).ok();
                return false;
            }
            std::fs::remove_dir_all(&temp_dir).ok();
            return true;
        }

        println!("cargo:warning=Downloaded ruff archive did not contain ruff binary");
        std::fs::remove_dir_all(&temp_dir).ok();
        false
    }

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rustc-env=RUFF_VERSION={}", super::RUFF_VERSION);

        let target = build_helpers::build_target_triple();

        // Architecture-aware cache to avoid sharing binaries between platforms.
        // The cache key is profile-independent: the downloaded ruff binary is the
        // same regardless of debug/release, and `cargo install` always builds in
        // release mode.
        let cache_dir = build_helpers::cache_dir(&format!("ruff-{}", super::RUFF_VERSION));
        let cached_ruff_path = cache_dir.join(format!("ruff-{target}"));

        let out_dir = env::var("OUT_DIR").unwrap();
        let ruff_binary_path = format!("{}/ruff", out_dir);

        // 1. Check architecture-aware cache
        if cached_ruff_path.exists() {
            println!(
                "cargo:warning=Using cached ruff binary from {:?}",
                cached_ruff_path
            );
            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("Failed to copy cached ruff binary");
            return;
        }

        // 2. Download pre-built binary from GitHub releases
        if !download_ruff(&target, &cached_ruff_path) {
            // 3. Compile from source as fallback
            println!(
                "cargo:warning=Pre-built ruff not available for {target}, compiling from source..."
            );
            match build_helpers::cargo_install_binary(
                "ruff",
                super::RUFF_VERSION,
                &target,
                &cache_dir,
            ) {
                Some(compiled) => {
                    std::fs::copy(&compiled, &cached_ruff_path)
                        .expect("Failed to copy compiled ruff binary to cache");
                }
                None => panic!(
                    "Failed to download or compile ruff {} for {target}. \
                     Ensure a Rust toolchain with the {target} target is installed, \
                     or check https://github.com/astral-sh/ruff/releases/tag/{}",
                    super::RUFF_VERSION,
                    super::RUFF_VERSION,
                ),
            }
        }

        std::fs::copy(&cached_ruff_path, &ruff_binary_path)
            .expect("Failed to copy ruff binary to OUT_DIR");
    }
}

mod peppylib_build {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Returns the platform suffix for the host machine (e.g. "macos-aarch64", "linux-x86_64").
    ///
    /// Uses `std::env::consts` which always reflects the machine running the build script,
    /// not the cross-compile target. This is correct because pixi/maturin produces a `.so`
    /// for the host regardless of Cargo's `--target`.
    fn host_platform_suffix() -> String {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        format!("{os}-{arch}")
    }

    /// Runs a pixi task and panics on failure.
    fn run_pixi_task(peppylib_py_dir: &Path, task: &str, target_dir: &Path) {
        let output = Command::new("sh")
            .args([
                "-c",
                &format!("ulimit -n 10240 && exec pixi run -e default {task}"),
            ])
            .current_dir(peppylib_py_dir)
            .env("CARGO_TARGET_DIR", target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .stdin(std::process::Stdio::null())
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
    #[cfg(target_os = "macos")]
    fn extract_so_from_wheel(wheels_dir: &Path) -> Vec<u8> {
        let whl_path = std::fs::read_dir(wheels_dir)
            .expect("failed to read wheels directory")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "whl"))
            .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
            .map(|e| e.path())
            .expect("no .whl file found in wheels directory");

        let file = std::fs::File::open(&whl_path).expect("failed to open wheel file");
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

    /// A Linux cross-compilation target for peppylib.
    #[cfg(target_os = "macos")]
    struct LinuxCrossTarget {
        /// The Rust target triple (e.g. "aarch64-unknown-linux-gnu").
        target_triple: &'static str,
        /// The pixi task name for this target.
        pixi_task: &'static str,
        /// The platform suffix for the output .so file (e.g. "linux-aarch64").
        platform_suffix: &'static str,
    }

    #[cfg(target_os = "macos")]
    const LINUX_CROSS_TARGETS: &[LinuxCrossTarget] = &[
        LinuxCrossTarget {
            target_triple: "aarch64-unknown-linux-gnu",
            pixi_task: "cross-linux-aarch64-release",
            platform_suffix: "linux-aarch64",
        },
        LinuxCrossTarget {
            target_triple: "x86_64-unknown-linux-gnu",
            pixi_task: "cross-linux-x86_64-release",
            platform_suffix: "linux-x86_64",
        },
    ];

    /// Ensures the given Rust target is installed via rustup.
    #[cfg(target_os = "macos")]
    fn ensure_linux_rust_target(target_triple: &str) {
        let status = Command::new("rustup")
            .args(["target", "add", target_triple])
            .status()
            .expect("failed to run rustup target add");
        if !status.success() {
            panic!("rustup target add {target_triple} failed");
        }
    }

    /// Returns true if maturin needs to run: the native `.so` is absent or older
    /// than any source file in peppylib-py (`src/**`, `peppylib/**/*.py`, `pixi.lock`)
    /// or its dependency crates (`peppylib`, `config-internal`, `pmi-internal`).
    fn so_needs_rebuild(peppylib_py_dir: &Path, peppylib_dir: &Path) -> bool {
        // Use the newest existing platform-suffixed .so as the reference timestamp.
        let so_mtime = std::fs::read_dir(peppylib_dir).ok().and_then(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let s = n.to_string_lossy();
                    s.starts_with("_peppylib.abi3.") && s.ends_with(".so")
                })
                .filter_map(|e| e.metadata().ok()?.modified().ok())
                .max()
        });
        let Some(so_mtime) = so_mtime else {
            return true;
        };

        let newer = |path: &Path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t > so_mtime)
        };

        let src_dir = peppylib_py_dir.join("src");
        if src_dir.is_dir() && super::walkdir(&src_dir).iter().any(|f| newer(f)) {
            return true;
        }

        let py_dir = peppylib_py_dir.join("peppylib");
        if py_dir.is_dir() {
            let changed = super::walkdir(&py_dir)
                .into_iter()
                .filter(|f| {
                    !f.components().any(|c| c.as_os_str() == "__pycache__")
                        && f.extension().is_some_and(|e| e == "py")
                })
                .any(|f| newer(&f));
            if changed {
                return true;
            }
        }

        // Check dependency crate sources that are compiled into the .so
        let crates_root = peppylib_py_dir.join("..");
        for dep_crate in &["peppylib", "config-internal", "pmi-internal"] {
            let dep_src = crates_root.join(dep_crate).join("src");
            if dep_src.is_dir() && super::walkdir(&dep_src).iter().any(|f| newer(f)) {
                return true;
            }
        }

        newer(&peppylib_py_dir.join("pixi.lock"))
    }

    fn register_rerun_triggers(peppylib_py_dir: &Path) {
        println!("cargo:rerun-if-changed=../peppylib-py/Cargo.toml");
        let src_dir = peppylib_py_dir.join("src");
        if src_dir.is_dir() {
            for entry in super::walkdir(&src_dir) {
                println!("cargo:rerun-if-changed={}", entry.display());
            }
        }

        // Watch .py source files in the peppylib package directory.
        let peppylib_dir = peppylib_py_dir.join("peppylib");
        if peppylib_dir.is_dir() {
            for entry in super::walkdir(&peppylib_dir) {
                if entry.components().any(|c| c.as_os_str() == "__pycache__") {
                    continue;
                }
                if entry.extension().is_some_and(|ext| ext == "py") {
                    println!("cargo:rerun-if-changed={}", entry.display());
                }
            }
        }
    }

    fn resolve_pixi_task() -> &'static str {
        let profile = std::env::var("PROFILE").unwrap();
        if profile == "release" {
            "release"
        } else {
            "dev"
        }
    }

    /// Returns true if `pixi` is available on PATH.
    fn is_pixi_available() -> bool {
        Command::new("pixi")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Builds the native `.so` via pixi and renames it to a platform-suffixed name.
    fn build_native_so(
        peppylib_py_dir: &Path,
        peppylib_dir: &Path,
        so_path: &Path,
        pixi_task: &str,
        target_dir: &Path,
    ) {
        // Serialize concurrent pixi invocations to avoid "Text file busy" races
        // when multiple build scripts run pixi on the same environment.
        let lock_path = peppylib_py_dir.join(".pixi/.build.lock");
        let _pixi_lock = build_helpers::acquire_file_lock(&lock_path);

        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");
        run_pixi_task(peppylib_py_dir, pixi_task, target_dir);

        assert!(
            so_path.exists(),
            "Expected _peppylib.abi3.so at {:?} after pixi run {pixi_task}, but not found",
            so_path,
        );

        let host_suffix = host_platform_suffix();
        let native_so_path = peppylib_dir.join(format!("_peppylib.abi3.{host_suffix}.so"));
        std::fs::rename(so_path, &native_so_path).unwrap_or_else(|e| {
            panic!(
                "failed to rename {:?} to {:?}: {e}",
                so_path, native_so_path
            )
        });
    }

    /// Cross-compiles a Linux `.so` via maturin + zig for the given target.
    ///
    /// Always uses release mode — the cross-compiled `.so` is a container deployment
    /// artifact that never needs debug symbols, and debug builds are ~4x larger.
    #[cfg(target_os = "macos")]
    fn cross_compile_linux_so(
        target: &LinuxCrossTarget,
        peppylib_py_dir: &Path,
        target_dir: &Path,
        peppylib_dir: &Path,
    ) {
        println!(
            "cargo:warning=Cross-compiling peppylib-py for {} via pixi ({})…",
            target.platform_suffix, target.pixi_task
        );

        ensure_linux_rust_target(target.target_triple);
        run_pixi_task(peppylib_py_dir, target.pixi_task, target_dir);

        let wheels_dir = target_dir.join("wheels");
        let linux_so_bytes = extract_so_from_wheel(&wheels_dir);

        let linux_so_path =
            peppylib_dir.join(format!("_peppylib.abi3.{}.so", target.platform_suffix));
        std::fs::write(&linux_so_path, &linux_so_bytes)
            .unwrap_or_else(|e| panic!("failed to write linux .so to {:?}: {e}", linux_so_path));

        // Clean up wheel files to avoid stale artifacts between targets
        std::fs::remove_dir_all(&wheels_dir).ok();
    }

    /// Marker file that records the source hash at the time the `.so` was built.
    const SOURCE_HASH_MARKER: &str = ".so-source-hash";

    /// Dependency crate source directories that are compiled into the `.so`.
    const DEP_CRATES: &[&str] = &["peppylib", "config-internal", "pmi-internal"];

    /// Computes a SHA-256 hash over all source files that feed into the `.so`
    /// build: peppylib-py Rust sources, Python sources, pixi.lock, and
    /// dependency crate sources. Used to detect branch-switch staleness that
    /// mtime-based checks miss.
    fn compute_source_hash(peppylib_py_dir: &Path) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();

        let mut collect_dir = |dir: &Path, filter_py: bool| {
            if !dir.is_dir() {
                return;
            }
            let mut files = super::walkdir(dir);
            files.sort();
            for f in &files {
                if f.components().any(|c| c.as_os_str() == "__pycache__") {
                    continue;
                }
                if filter_py && !f.extension().is_some_and(|e| e == "py") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(f) {
                    hasher.update(f.file_name().unwrap_or_default().as_encoded_bytes());
                    hasher.update(&bytes);
                }
            }
        };

        // peppylib-py Rust sources
        collect_dir(&peppylib_py_dir.join("src"), false);
        // peppylib-py Python sources
        collect_dir(&peppylib_py_dir.join("peppylib"), true);

        // Dependency crate sources
        let crates_root = peppylib_py_dir.join("..");
        for dep_crate in DEP_CRATES {
            collect_dir(&crates_root.join(dep_crate).join("src"), false);
        }

        // pixi.lock
        if let Ok(bytes) = std::fs::read(peppylib_py_dir.join("pixi.lock")) {
            hasher.update(b"pixi.lock");
            hasher.update(&bytes);
        }

        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Returns `true` if the source hash marker is missing or doesn't match the
    /// current source hash — indicating the `.so` files are stale.
    fn source_hash_changed(peppylib_py_dir: &Path, peppylib_dir: &Path) -> bool {
        let marker_path = peppylib_dir.join(SOURCE_HASH_MARKER);
        let Ok(saved) = std::fs::read_to_string(&marker_path) else {
            return true; // No marker = assume stale
        };
        let current = compute_source_hash(peppylib_py_dir);
        saved.trim() != current
    }

    /// Writes the source hash marker after a successful build.
    fn write_source_hash_marker(peppylib_py_dir: &Path, peppylib_dir: &Path) {
        let hash = compute_source_hash(peppylib_py_dir);
        let marker_path = peppylib_dir.join(SOURCE_HASH_MARKER);
        std::fs::write(&marker_path, &hash).unwrap_or_else(|e| {
            println!(
                "cargo:warning=Failed to write source hash marker {:?}: {e}",
                marker_path
            );
        });
    }

    /// Deletes all platform `.so` files so the next build starts fresh.
    fn remove_stale_so_files(peppylib_dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(peppylib_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s.starts_with("_peppylib.abi3.") && s.ends_with(".so") {
                    std::fs::remove_file(entry.path()).ok();
                }
            }
        }
    }

    /// Computes a combined SHA-256 hash of all platform `.so` files and emits it
    /// as the `PEPPYLIB_SO_HASH` env var for cache invalidation.
    fn compute_and_emit_so_hash(peppylib_dir: &Path) {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        let mut so_files: Vec<_> = std::fs::read_dir(peppylib_dir)
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

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");
        let peppylib_dir = peppylib_py_dir.join("peppylib");
        let so_path = peppylib_dir.join("_peppylib.abi3.so");

        register_rerun_triggers(&peppylib_py_dir);
        println!(
            "cargo:rerun-if-changed={}",
            peppylib_dir.join(SOURCE_HASH_MARKER).display()
        );

        // Use a separate CARGO_TARGET_DIR so maturin's inner `cargo build`
        // does not deadlock on the workspace build lock held by the outer cargo.
        let cache_dir = build_helpers::cache_dir("peppylib-py");
        let target_dir = cache_dir.join("target");
        let pixi_task = resolve_pixi_task();

        // Skip peppylib build when pixi is unavailable.
        // When skipped, the hash is computed from whatever .so files already
        // exist (e.g. from a prior build).
        if !is_pixi_available() {
            if source_hash_changed(&peppylib_py_dir, &peppylib_dir) {
                panic!(
                    "Stale peppylib-py .so files: sources have changed since last build \
                     but pixi is not available to rebuild. Run \
                     `cargo build -p generator` on a machine with pixi first."
                );
            }
            println!(
                "cargo:warning=Skipping peppylib-py build (pixi not available). \
                 Using existing .so files."
            );
        } else {
            // Use both mtime and content-hash checks. The hash check catches
            // branch-switch staleness that mtime comparison misses (the .so
            // may be newer than re-checked-out source files).
            let sources_changed = so_needs_rebuild(&peppylib_py_dir, &peppylib_dir)
                || source_hash_changed(&peppylib_py_dir, &peppylib_dir);

            if sources_changed {
                // Delete stale .so files before rebuilding so we start fresh.
                remove_stale_so_files(&peppylib_dir);

                build_native_so(
                    &peppylib_py_dir,
                    &peppylib_dir,
                    &so_path,
                    pixi_task,
                    &target_dir,
                );
            } else {
                println!("cargo:warning=Skipping peppylib-py native build (sources unchanged).");
            }

            // Cross-compile for each Linux target if sources changed OR if
            // the target's .so is missing (e.g. a new platform was added).
            #[cfg(target_os = "macos")]
            for target in LINUX_CROSS_TARGETS {
                let target_so =
                    peppylib_dir.join(format!("_peppylib.abi3.{}.so", target.platform_suffix));
                if sources_changed || !target_so.exists() {
                    cross_compile_linux_so(target, &peppylib_py_dir, &target_dir, &peppylib_dir);
                }
            }

            // Record the source hash so future builds can detect staleness.
            write_source_hash_marker(&peppylib_py_dir, &peppylib_dir);
        }

        // Guard against partial rebuilds leaving an unsuffixed .so behind
        if so_path.exists() {
            std::fs::remove_file(&so_path).ok();
        }

        compute_and_emit_so_hash(&peppylib_dir);
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
                    println!(
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
            ("../build-helpers-internal", false),
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
                    Err(e) => println!(
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
