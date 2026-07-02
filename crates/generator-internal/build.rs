/// Pinned ruff release tag used when building from source.
const RUFF_VERSION: &str = "0.15.14";

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

/// Returns true if a file inside a Rust crate is source that affects the crate's
/// compiled output: `.rs`, `.toml`, `.capnp`, `.j2`, plus config's
/// `tools/capnp_*` helpers. `relative_path` is relative to the crate root.
/// Mirrors the rust-embed include/exclude rules used when embedding crate source
/// for node scaffolding, so the staleness hash and the embed stay in agreement.
fn is_crate_source_file(relative_path: &str, is_config_internal: bool) -> bool {
    if relative_path.starts_with("target/")
        || relative_path.starts_with("tests/")
        || relative_path.starts_with("examples/")
    {
        return false;
    }

    if relative_path.ends_with(".rs")
        || relative_path.ends_with(".toml")
        || relative_path.ends_with(".capnp")
        || relative_path.ends_with(".j2")
    {
        return true;
    }

    is_config_internal && relative_path.starts_with("tools/capnp_")
}

/// Recursively collects every compilation-relevant source file under a crate
/// directory, skipping the `target`, `tests`, and `examples` subdirectories.
/// Paths are returned unsorted; callers that need determinism sort the result.
fn collect_crate_source_files(
    crate_dir: &std::path::Path,
    is_config_internal: bool,
) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![crate_dir.to_path_buf()];

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
            let rel = path.strip_prefix(crate_dir).unwrap_or(&path);
            let rel_str = rel.to_string_lossy();
            if path.is_dir() {
                // Only top-level target/tests/examples are excluded.
                if rel_str == "target" || rel_str == "tests" || rel_str == "examples" {
                    continue;
                }
                stack.push(path);
            } else if is_crate_source_file(&rel_str, is_config_internal) {
                files.push(path);
            }
        }
    }

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
            build_helpers::copy_if_changed(&cached_ruff_path, ruff_binary_path.as_ref());
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

        build_helpers::copy_if_changed(&cached_ruff_path, ruff_binary_path.as_ref());
    }
}

mod peppylib_build {
    use std::collections::BTreeMap;
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

    /// Directory served as `PIXI_HOME` to every pixi invocation: holds a
    /// `config.toml` that detaches pixi environments into the peppy cache dir.
    /// Without it pixi materializes `.pixi/envs` (hundreds of MB) inside
    /// peppylib-py's directory, which for release builds is the immutable,
    /// machine-shared cargo checkout of public-peppy-libs. Detached
    /// environments are keyed by manifest path, so distinct checkouts still
    /// get distinct environments.
    fn pixi_home_with_detached_envs() -> PathBuf {
        let cache = build_helpers::cache_dir("peppylib-py");
        let pixi_home = cache.join("pixi-home");
        std::fs::create_dir_all(&pixi_home)
            .unwrap_or_else(|e| panic!("failed to create pixi home {pixi_home:?}: {e}"));
        let config = format!(
            "detached-environments = \"{}\"\n",
            cache.join("pixi-envs").display()
        );
        build_helpers::write_if_changed(&pixi_home.join("config.toml"), config.as_bytes());
        pixi_home
    }

    /// Runs a pixi task and panics on failure.
    ///
    /// Runs with `--frozen` so pixi installs exactly the committed `pixi.lock`
    /// and never rewrites it. peppylib-py is usually read from an immutable
    /// cargo checkout that the Lima VM build resolves independently; an
    /// in-place re-lock (for example from a pixi too old for the lock format)
    /// would make the host and VM disagree about the sources and poison the
    /// recorded `.so` hash.
    fn run_pixi_task(peppylib_py_dir: &Path, task: &str, target_dir: &Path) {
        let output = Command::new("sh")
            .args([
                "-c",
                &format!("ulimit -n 10240 && exec pixi run --frozen -e default {task}"),
            ])
            .current_dir(peppylib_py_dir)
            .env("CARGO_TARGET_DIR", target_dir)
            .env("PIXI_HOME", pixi_home_with_detached_envs())
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

    /// All `.py` files in the peppylib package directory, excluding `__pycache__`.
    /// These do not feed the compiled `.so` but are embedded alongside it, so a
    /// change must refresh the `PEPPYLIB_SO_HASH` embed cache key.
    fn peppylib_python_files(peppylib_dir: &Path) -> Vec<PathBuf> {
        if !peppylib_dir.is_dir() {
            return Vec::new();
        }
        super::walkdir(peppylib_dir)
            .into_iter()
            .filter(|f| {
                !f.components().any(|c| c.as_os_str() == "__pycache__")
                    && f.extension().is_some_and(|e| e == "py")
            })
            .collect()
    }

    /// Registers every input that should rerun this build script: each file that
    /// feeds the compiled `.so` (so a dep-crate edit retriggers the staleness
    /// check), each `.py` file in the peppylib package (so a Python edit refreshes
    /// the embed key), and the manual-refresh override env var.
    fn register_rerun_triggers(peppylib_py_dir: &Path) {
        for file in so_rebuild_input_files(peppylib_py_dir) {
            println!("cargo:rerun-if-changed={}", file.display());
        }
        for py_file in peppylib_python_files(&peppylib_py_dir.join("peppylib")) {
            println!("cargo:rerun-if-changed={}", py_file.display());
        }
        println!("cargo:rerun-if-env-changed={REBUILD_ENV_VAR}");
        println!("cargo:rerun-if-env-changed={PREBUILT_SO_DIR_ENV}");
        // Toggling the cross-build flag must re-run build.rs so the Linux .so are
        // (re)built or skipped to match, instead of replaying a stale decision.
        println!("cargo:rerun-if-env-changed=PEPPY_CROSS_BUILD");
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
        so_dir: &Path,
        so_path: &Path,
        pixi_task: &str,
        target_dir: &Path,
    ) {
        // Serialize concurrent pixi invocations to avoid "Text file busy" races
        // when multiple build scripts run pixi on the same environment. Lives in
        // the peppy cache dir next to the detached environments, never inside
        // the (possibly immutable) peppylib-py checkout.
        let lock_path = build_helpers::cache_dir("peppylib-py").join("pixi-build.lock");
        let _pixi_lock = build_helpers::acquire_file_lock(&lock_path);

        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");
        run_pixi_task(peppylib_py_dir, pixi_task, target_dir);

        assert!(
            so_path.exists(),
            "Expected _peppylib.abi3.so at {:?} after pixi run {pixi_task}, but not found",
            so_path,
        );

        let host_suffix = host_platform_suffix();
        let native_so_path = so_dir.join(format!("_peppylib.abi3.{host_suffix}.so"));
        std::fs::rename(so_path, &native_so_path).unwrap_or_else(|e| {
            panic!(
                "failed to rename {:?} to {:?}: {e}",
                so_path, native_so_path
            )
        });

        // Strip debug info from the embedded native extension. This `.so` is a
        // pure runtime artifact; it is baked into the generator binary and
        // vendored into every generated node's `.peppy/libs/peppylib`, where uv
        // then caches one wheel per node build. Under the `dev` profile the
        // unstripped debug `.so` is ~457 MB (vs ~104 MB after `-S`), which
        // dominated both the generator binary and the uv cache. `-S` drops the
        // DWARF debuginfo (the bulk) while keeping the symbol table, so native
        // backtraces still resolve and the exported `PyInit__peppylib` symbol is
        // untouched. Best-effort: a missing `strip` just leaves the larger `.so`.
        // Release builds are already stripped via `[profile.release] strip` in
        // the workspace Cargo.toml, so this mainly bites `cargo test` (debug).
        let _ = Command::new("strip")
            .arg("-S")
            .arg(&native_so_path)
            .status();
    }

    /// Cross-compiles a Linux `.so` via maturin + zig for the given target.
    ///
    /// Always uses release mode: the cross-compiled `.so` is a container deployment
    /// artifact that never needs debug symbols, and debug builds are ~4x larger.
    #[cfg(target_os = "macos")]
    fn cross_compile_linux_so(
        target: &LinuxCrossTarget,
        peppylib_py_dir: &Path,
        target_dir: &Path,
        so_dir: &Path,
    ) {
        println!(
            "cargo:warning=Cross-compiling peppylib-py for {} via pixi ({})…",
            target.platform_suffix, target.pixi_task
        );

        ensure_linux_rust_target(target.target_triple);
        run_pixi_task(peppylib_py_dir, target.pixi_task, target_dir);

        let wheels_dir = target_dir.join("wheels");
        let linux_so_bytes = extract_so_from_wheel(&wheels_dir);

        let linux_so_path = so_dir.join(format!("_peppylib.abi3.{}.so", target.platform_suffix));
        std::fs::write(&linux_so_path, &linux_so_bytes)
            .unwrap_or_else(|e| panic!("failed to write linux .so to {:?}: {e}", linux_so_path));

        // Clean up wheel files to avoid stale artifacts between targets
        std::fs::remove_dir_all(&wheels_dir).ok();
    }

    /// Marker file recording, per platform, the source hash and profile each
    /// embedded `.so` was built from. Tracking state per artifact lets the host
    /// and Linux `.so` files go stale independently: the host always rebuilds,
    /// while a stale Linux `.so` is left alone on debug builds.
    const BUILD_STATE_MARKER: &str = ".so-build-state";

    /// Env var that forces a full rebuild (including the Linux cross-compile) on
    /// a debug build. Set by the release build path and available to developers
    /// iterating on container bindings.
    const REBUILD_ENV_VAR: &str = "PEPPYLIB_REBUILD";

    /// Env var pointing at a directory of host-built `.so` to consume read-only
    /// instead of building. Set by the release cross-build inside the Lima VM,
    /// which has no pixi: the host build already cross-compiled every platform's
    /// binding into a peppy-owned cache dir (mounted into the VM), so the in-VM
    /// build embeds those directly rather than fetching and seeding a cargo
    /// checkout. When set, no pixi is invoked and no `.so` is written anywhere.
    const PREBUILT_SO_DIR_ENV: &str = "PEPPYLIB_PREBUILT_SO_DIR";

    /// True for a platform-suffixed native extension (`_peppylib.abi3.<plat>.so`),
    /// excluding the canonical `_peppylib.abi3.so` Python imports at runtime.
    fn is_platform_so_name(name: &str) -> bool {
        name.starts_with("_peppylib.abi3.") && name.ends_with(".so") && name != "_peppylib.abi3.so"
    }

    /// This build script's own source, embedded so it can be mixed into every
    /// `.so` hash. The build logic is itself an input to the compiled artifact:
    /// a change to how the `.so` is built, or to how its staleness is judged, can
    /// change the output, so it must invalidate the cached `.so` and its
    /// `.so-build-state` marker. Folding the script's source into the hash does
    /// that automatically on the next build, with no version constant for anyone
    /// to remember to bump. `include_str!` resolves relative to this file, so it
    /// always embeds `generator-internal/build.rs`.
    const BUILD_SCRIPT_SOURCE: &str = include_str!("build.rs");

    /// Dependency crates compiled into the `.so`, paired with whether the crate
    /// is config (which embeds extra `tools/capnp_*` helpers). This list
    /// MUST stay in sync with peppylib-py's path dependencies in
    /// `public-peppy-libs/peppyos-shared/peppylib-py/Cargo.toml`; a crate
    /// missing here means edits to it
    /// silently produce a stale `.so`.
    const SO_DEP_CRATES: &[(&str, bool)] = &[
        // Resolved against `peppyos-shared` (located via build-helpers); all are
        // siblings of peppylib-py in that shared workspace.
        ("peppylib-rs", false),
        ("peppy-config-model", true),
        ("peppy-messaging-interface", false),
        ("core-node-api", false),
    ];

    /// Every source file of the `.so` dependency crates. Resolved against
    /// `peppyos-shared`, located via build-helpers so it works in the superproject
    /// and from a cargo git checkout of public-peppy-libs alike.
    fn dep_crate_source_files() -> Vec<PathBuf> {
        let crates_root = build_helpers::peppyos_shared_dir();
        let mut files = Vec::new();
        for (crate_name, is_config) in SO_DEP_CRATES {
            files.extend(super::collect_crate_source_files(
                &crates_root.join(crate_name),
                *is_config,
            ));
        }
        files
    }

    /// Every existing file whose contents are compiled into the `.so`: peppylib-py's
    /// own Rust bindings and build manifests, plus the source of every dependency
    /// crate. This is the single source of truth shared by the content hash and the
    /// rerun registration so they cannot drift apart. The peppylib package `.py`
    /// files are deliberately excluded; they are embedded for scaffolding but
    /// never affect the compiled binary.
    fn so_rebuild_input_files(peppylib_py_dir: &Path) -> Vec<PathBuf> {
        let mut files = super::collect_crate_source_files(&peppylib_py_dir.join("src"), false);

        for manifest in ["Cargo.toml", "pyproject.toml", "pixi.toml", "pixi.lock"] {
            let path = peppylib_py_dir.join(manifest);
            if path.is_file() {
                files.push(path);
            }
        }

        files.extend(dep_crate_source_files());

        files.sort();
        files
    }

    /// Computes a SHA-256 hash over this build script's source plus the given
    /// input files, keyed by each file's path relative to the crates root so a
    /// move is detected and same-named files in different crates never collide.
    /// The build script's source is folded in first so a change to the build logic
    /// invalidates the cached `.so` (see [`BUILD_SCRIPT_SOURCE`]).
    ///
    /// Keys are relative to `peppyos-shared` (where every input, the dependency
    /// crates and peppylib-py itself, lives). The marker files are per-checkout and
    /// gitignored, so keys only need to be stable and collision-free within a
    /// single checkout.
    fn hash_files(files: &[PathBuf]) -> String {
        use sha2::{Digest, Sha256};

        let crates_root = build_helpers::peppyos_shared_dir();
        let mut hasher = Sha256::new();
        hasher.update(BUILD_SCRIPT_SOURCE.as_bytes());
        for file in files {
            let rel = file.strip_prefix(&crates_root).unwrap_or(file);
            if let Ok(bytes) = std::fs::read(file) {
                hasher.update(rel.to_string_lossy().as_bytes());
                hasher.update(&bytes);
            }
        }
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Hash over every `.so` input file (peppylib-py plus its dependency crates).
    /// Drives the per-platform rebuild decision: any change rebuilds the host `.so`.
    fn compute_source_hash(peppylib_py_dir: &Path) -> String {
        hash_files(&so_rebuild_input_files(peppylib_py_dir))
    }

    /// Hash over only the dependency crate sources, with peppylib-py's own code
    /// excluded. Keys the isolated maturin target directory: when the dependency
    /// crates change, the build moves to a fresh target tree so it can never link
    /// a stale rlib that cargo's mtime fingerprint failed to invalidate across a
    /// git-checkout swap. Keeping it separate from the full source hash means
    /// iterating on peppylib-py's own bindings reuses the same warm target.
    fn compute_dep_hash() -> String {
        let mut files = dep_crate_source_files();
        files.sort();
        hash_files(&files)
    }

    /// Reads the per-platform build state, mapping each platform suffix to the
    /// `(source_hash, profile)` its `.so` was last built from. A missing or
    /// malformed marker parses to an empty map (everything treated as stale).
    fn read_build_state(so_dir: &Path) -> BTreeMap<String, (String, String)> {
        let Ok(contents) = std::fs::read_to_string(so_dir.join(BUILD_STATE_MARKER)) else {
            return BTreeMap::new();
        };
        contents
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let platform = fields.next()?.to_string();
                let hash = fields.next()?.to_string();
                let profile = fields.next()?.to_string();
                Some((platform, (hash, profile)))
            })
            .collect()
    }

    /// Writes the per-platform build state, using write_if_changed to avoid
    /// touching the file (and its mtime) on no-op builds.
    fn write_build_state(so_dir: &Path, state: &BTreeMap<String, (String, String)>) {
        let body: String = state
            .iter()
            .map(|(platform, (hash, profile))| format!("{platform}\t{hash}\t{profile}\n"))
            .collect();
        build_helpers::write_if_changed(&so_dir.join(BUILD_STATE_MARKER), body.as_bytes());
    }

    /// Removes `.so` files (and their state entries) for platforms that are no
    /// longer built, so a removed target does not leave a stale artifact behind.
    fn prune_orphan_so_files(
        so_dir: &Path,
        state: &mut BTreeMap<String, (String, String)>,
        current_platforms: &[String],
    ) {
        let Ok(entries) = std::fs::read_dir(so_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let suffix = name
                .to_string_lossy()
                .strip_prefix("_peppylib.abi3.")
                .and_then(|s| s.strip_suffix(".so"))
                .map(str::to_string);
            if let Some(suffix) = suffix
                && !current_platforms.contains(&suffix)
            {
                std::fs::remove_file(entry.path()).ok();
            }
        }
        state.retain(|platform, _| current_platforms.contains(platform));
    }

    /// Computes a combined SHA-256 hash of all platform `.so` files **and**
    /// every `.py` file in the peppylib package, emitting it as the
    /// `PEPPYLIB_SO_HASH` env var for cache invalidation. The `.py` wrappers live
    /// in the source checkout (`peppylib_dir`); the compiled `.so` live in the
    /// peppy-owned `so_dir`. Including the `.py` files ensures a Python-side edit
    /// (rename, new export, etc.) invalidates the cached embed even when the
    /// native `.so` is unchanged.
    fn compute_and_emit_so_hash(peppylib_dir: &Path, so_dir: &Path) {
        use sha2::{Digest, Sha256};

        let mut hashed_files: Vec<PathBuf> = super::walkdir(peppylib_dir)
            .into_iter()
            .filter(|p| {
                !p.components().any(|c| c.as_os_str() == "__pycache__")
                    && p.extension().is_some_and(|ext| ext == "py")
            })
            .collect();
        hashed_files.extend(super::walkdir(so_dir).into_iter().filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_platform_so_name)
        }));
        // Keyed by filename: the `.py` and `.so` come from two dirs but their
        // names never collide, and the pre-split layout sorted by path within one
        // dir, so filename order keeps the hash stable and deterministic.
        hashed_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let mut hasher = Sha256::new();
        for file in &hashed_files {
            let bytes = std::fs::read(file)
                .unwrap_or_else(|e| panic!("failed to read {:?} for hashing: {e}", file));
            hasher.update(file.file_name().unwrap().as_encoded_bytes());
            hasher.update(&bytes);
        }
        let hash = hasher.finalize();
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        println!("cargo:rustc-env=PEPPYLIB_SO_HASH={}", &hex[..16]);
    }

    /// Emits `embedded_peppylib_so.rs` into OUT_DIR: a `PEPPYLIB_PLATFORM_SO`
    /// table mapping each platform-suffixed `.so` filename to its bytes via
    /// `include_bytes!`. The scaffolder consumes this table to write the target
    /// platform's native extension. This mirrors [`embed_ruff_binary`]: build
    /// artifacts are embedded through OUT_DIR rather than a compile-time source
    /// path, so the `.so` can live in a peppy-owned cache dir instead of being
    /// written into the shared source checkout.
    fn generate_embedded_peppylib_so(so_dir: &Path) {
        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let generated = out_dir.join("embedded_peppylib_so.rs");

        let mut so_files: Vec<PathBuf> = std::fs::read_dir(so_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_platform_so_name)
            })
            .collect();
        so_files.sort();

        let mut entries = String::new();
        for so in &so_files {
            let name = so.file_name().and_then(|n| n.to_str()).unwrap();
            entries.push_str(&format!("    ({name:?}, include_bytes!({so:?})),\n"));
            // A changed `.so` must recompile the scaffolder that embeds it.
            println!("cargo:rerun-if-changed={}", so.display());
        }

        let content = format!(
            "// @generated by generator-internal/build.rs. Platform-suffixed peppylib\n\
             // native extensions, embedded from a peppy-owned cache dir.\n\
             pub const PEPPYLIB_PLATFORM_SO: &[(&str, &[u8])] = &[\n{entries}];\n"
        );
        build_helpers::write_if_changed(&generated, content.as_bytes());
    }

    /// Verify the host-built `.so` a release cross-build is about to embed are
    /// present and were built from the current sources. Reads the same
    /// `.so-build-state` marker the producing build writes, and fails loudly on a
    /// missing, empty, or stale binding so a release can never ship bindings that
    /// mismatch the sources compiled around them.
    fn verify_prebuilt_so(so_dir: &Path, current_hash: &str) {
        let state = read_build_state(so_dir);
        let mut platform_so: Vec<String> = std::fs::read_dir(so_dir)
            .unwrap_or_else(|e| panic!("{PREBUILT_SO_DIR_ENV}={so_dir:?} cannot be read: {e}"))
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| is_platform_so_name(name))
            .collect();
        platform_so.sort();

        assert!(
            !platform_so.is_empty(),
            "{PREBUILT_SO_DIR_ENV}={so_dir:?} holds no peppylib .so; run the host \
             build before cross-building"
        );
        for name in &platform_so {
            let suffix = name
                .strip_prefix("_peppylib.abi3.")
                .and_then(|s| s.strip_suffix(".so"))
                .expect("is_platform_so_name guarantees the prefix and suffix");
            let recorded = state.get(suffix).map(|(h, _)| h.as_str());
            assert!(
                recorded == Some(current_hash),
                "prebuilt peppylib .so {name} in {so_dir:?} was built from stale \
                 sources (recorded {recorded:?}, current {current_hash}); rebuild \
                 the host artifacts before cross-building. If a rebuild does not \
                 fix this, the host and VM copies of peppyos-shared differ for \
                 the same pinned revision: run `git status` in the cargo checkout \
                 of public-peppy-libs on both sides to find local modifications"
            );
        }
    }

    pub fn run() {
        // peppylib-py and its `.so` dependency crates live in the shared
        // workspace (peppyos-shared), located via build-helpers so every path
        // resolves in the superproject and from a cargo git checkout of
        // public-peppy-libs alike. Only the Python wrappers are read from here;
        // the compiled `.so` are produced into a peppy-owned cache dir, never
        // written back into the immutable, cross-run cargo checkout.
        let peppylib_py_dir = build_helpers::peppyos_shared_dir().join("peppylib-py");
        let peppylib_dir = peppylib_py_dir.join("peppylib");

        register_rerun_triggers(&peppylib_py_dir);

        let current_hash = compute_source_hash(&peppylib_py_dir);

        // Release cross-builds run inside the Lima VM, which has no pixi and must
        // not touch the cargo checkout. They consume the host-built `.so` from an
        // explicit directory (mounted from the host) instead of building.
        if let Some(prebuilt) = std::env::var_os(PREBUILT_SO_DIR_ENV) {
            let so_dir = PathBuf::from(prebuilt);
            verify_prebuilt_so(&so_dir, &current_hash);
            generate_embedded_peppylib_so(&so_dir);
            compute_and_emit_so_hash(&peppylib_dir, &so_dir);
            return;
        }

        // Persistent, peppy-owned home for the built `.so` and their
        // `.so-build-state` marker, rooted outside the cargo checkout so release
        // staging never depends on cargo's git-cache layout (checkout dir names,
        // leftover checkouts from prior runs).
        let so_dir = build_helpers::cache_dir("peppylib-py").join("so");
        std::fs::create_dir_all(&so_dir)
            .unwrap_or_else(|e| panic!("failed to create peppylib .so dir {so_dir:?}: {e}"));

        let profile = peppylib_build_policy::BuildProfile::from_env();
        let host = host_platform_suffix();

        // Every platform this machine produces: the host always, plus the Linux
        // cross-compile targets on macOS.
        #[cfg(target_os = "macos")]
        let platforms: Vec<String> = std::iter::once(host.clone())
            .chain(
                LINUX_CROSS_TARGETS
                    .iter()
                    .map(|t| t.platform_suffix.to_string()),
            )
            .collect();
        #[cfg(not(target_os = "macos"))]
        let platforms: Vec<String> = vec![host.clone()];

        // The transient in-place output of `maturin develop`, which lands next to
        // the Python package in the checkout. It is moved into `so_dir` right
        // after the host build and removed below, so the checkout keeps no
        // artifact between runs.
        let so_path = peppylib_dir.join("_peppylib.abi3.so");

        // When pixi is unavailable we cannot rebuild, so fail only if an artifact
        // is actually missing or built from stale sources; otherwise serve what
        // exists.
        if !is_pixi_available() {
            let state = read_build_state(&so_dir);
            let needs_rebuild = platforms.iter().any(|p| {
                let so = so_dir.join(format!("_peppylib.abi3.{p}.so"));
                !so.exists() || state.get(p).map(|(h, _)| h.as_str()) != Some(current_hash.as_str())
            });
            assert!(
                !needs_rebuild,
                "Stale peppylib-py .so files: sources have changed since last build \
                 but pixi is not available to rebuild. Run \
                 `cargo build -p generator` on a machine with pixi first."
            );
            println!(
                "cargo:warning=Skipping peppylib-py build (pixi not available). \
                 Using existing .so files."
            );
            generate_embedded_peppylib_so(&so_dir);
            compute_and_emit_so_hash(&peppylib_dir, &so_dir);
            return;
        }

        // Isolate the maturin target by dependency-source hash. A change to one of
        // the `.so` dependency crates yields a different directory, so the build
        // always starts from a tree with no artifacts from a previous version of
        // those crates. This makes it impossible to link a stale dependency rlib
        // that cargo's mtime fingerprint failed to invalidate across a checkout
        // swap, the failure mode that produced a `.so` compiled against an
        // outdated `peppy_schema` enum. peppylib-py's own edits do not change this
        // hash, so iterating on the bindings reuses a warm target. A dedicated
        // CARGO_TARGET_DIR also keeps maturin's inner `cargo build` from
        // deadlocking on the workspace build lock held by the outer cargo. Old
        // `target-<hash>` trees from prior dependency versions persist per the
        // `cache_dir` convention (nothing auto-cleans it, and a sibling build in
        // another checkout may still be using one); remove them by hand with
        // `rm -rf ~/.peppy/tmp/peppylib-py/target-*` if disk use grows.
        let dep_hash = compute_dep_hash();
        let target_dir =
            build_helpers::cache_dir("peppylib-py").join(format!("target-{}", &dep_hash[..16]));
        let mut state = read_build_state(&so_dir);
        let force = std::env::var(REBUILD_ENV_VAR).is_ok_and(|v| !v.is_empty() && v != "0");

        // A forced rebuild discards the cached target so maturin recompiles every
        // crate from scratch: the guarantee the documented escape hatch provides
        // against any input the source hash does not cover.
        if force {
            match std::fs::remove_dir_all(&target_dir) {
                Ok(()) => {}
                // No cached target yet (e.g. first forced build): nothing to discard.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!(
                    "failed to clear maturin target dir {:?} for a forced rebuild \
                     ({REBUILD_ENV_VAR}): {e}",
                    target_dir
                ),
            }
        }

        // Host extension: rebuilt whenever it is missing, its recorded
        // (hash, profile) is stale, or a rebuild is forced. Built in both profiles
        // so local host scaffolding always matches current sources. The recorded
        // source hash already covers the dependency crates, so a dependency change
        // surfaces here as a stale hash and triggers a rebuild.
        let host_so = so_dir.join(format!("_peppylib.abi3.{host}.so"));
        let host_state = (current_hash.clone(), profile.tag().to_string());
        let host_current = state.get(&host) == Some(&host_state);
        if peppylib_build_policy::should_build_host(host_so.exists(), host_current, force) {
            build_native_so(
                &peppylib_py_dir,
                &so_dir,
                &so_path,
                profile.tag(),
                &target_dir,
            );
            state.insert(host.clone(), host_state);
        } else {
            println!("cargo:warning=Skipping peppylib-py host native build (sources unchanged).");
        }

        // Linux container bindings (macOS only). The build host is macOS, so
        // every Linux `.so` is a cross-compile, and all are release-only: like a
        // Linux `cargo build` (which builds only its own native host `.so`), a
        // regular build here produces only the host dynamic lib. The Linux
        // bindings are built solely under PEPPY_CROSS_BUILD (set by
        // scripts/build_release.sh and CI), so a plain `cargo build` never pays
        // the slow zig cross-compile and the two platforms stay consistent.
        #[cfg(target_os = "macos")]
        {
            let cross_build = peppylib_build_policy::cross_build_requested();
            for target in LINUX_CROSS_TARGETS {
                let suffix = target.platform_suffix;
                let target_so = so_dir.join(format!("_peppylib.abi3.{suffix}.so"));
                let target_state = (current_hash.clone(), "release".to_string());
                let stale = state.get(suffix) != Some(&target_state);
                if peppylib_build_policy::should_cross_compile(
                    cross_build,
                    target_so.exists(),
                    stale,
                    force,
                ) {
                    cross_compile_linux_so(target, &peppylib_py_dir, &target_dir, &so_dir);
                    state.insert(suffix.to_string(), target_state);
                }
            }
        }

        prune_orphan_so_files(&so_dir, &mut state, &platforms);
        write_build_state(&so_dir, &state);

        // Guard against `maturin develop` leaving its unsuffixed .so behind in the
        // checkout; the build artifact now lives in `so_dir`.
        if so_path.exists() {
            std::fs::remove_file(&so_path).ok();
        }

        generate_embedded_peppylib_so(&so_dir);
        compute_and_emit_so_hash(&peppylib_dir, &so_dir);
    }
}

/// Generates `embedded_ruff.rs` in OUT_DIR that embeds the ruff binary via `include_bytes!`.
/// This allows the binary to be extracted at runtime on any machine, rather than relying
/// on a stale compile-time filesystem path.
fn embed_ruff_binary() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ruff_binary_path = out_dir.join("ruff");
    let generated = out_dir.join("embedded_ruff.rs");

    let content = if ruff_binary_path.exists() {
        format!(
            r#"pub const RUFF_BINARY: Option<&[u8]> = Some(include_bytes!("{}"));"#,
            ruff_binary_path.display()
        )
    } else {
        r#"pub const RUFF_BINARY: Option<&[u8]> = None;"#.to_string()
    };

    // Use write_if_changed to avoid bumping mtime: this file is
    // referenced via include!() so any mtime change triggers recompilation.
    build_helpers::write_if_changed(&generated, content.as_bytes());
}

fn main() {
    // Single source of truth for the shared crate sources generator embeds: the
    // `peppyos-shared` dir located via build-helpers (works in-tree or from a
    // cargo git checkout). The rust-embed `#[folder = "$PEPPYOS_SHARED_DIR/…"]`
    // attributes in src/ expand this at compile time.
    println!(
        "cargo:rustc-env=PEPPYOS_SHARED_DIR={}",
        build_helpers::peppyos_shared_dir().display()
    );

    ruff_build::run();
    embed_ruff_binary();

    peppylib_build::run();
}
