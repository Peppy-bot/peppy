use std::fs::File;
use std::path::{Path, PathBuf};

/// Pinned ruff release tag used when building from source.
const RUFF_VERSION: &str = "0.15.0";

fn get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
    let temp_dir = std::env::temp_dir();
    let cache_dir = temp_dir.join(format!("{cache_suffix}-peppy-cache"));

    if !cache_dir.exists() {
        std::fs::create_dir_all(&cache_dir).expect("failed to create cache directory");
    }

    cache_dir
}

fn profile_target_subdir(profile: &str) -> &str {
    if profile == "release" {
        "release"
    } else {
        "debug"
    }
}

fn acquire_build_lock(lock_path: &Path) -> File {
    let lock_dir = lock_path
        .parent()
        .expect("lock path should include a parent directory");
    std::fs::create_dir_all(lock_dir).expect("failed to create lock directory");

    let lock_file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("failed to open lock file");

    lock_file.lock().expect("failed to acquire build lock");
    lock_file
}

fn copy_directory_recursive(from: &Path, to: &Path) {
    if to.exists() {
        std::fs::remove_dir_all(to).expect("failed to remove existing destination directory");
    }
    std::fs::create_dir_all(to).expect("failed to create destination directory");
    copy_directory_recursive_inner(from, to);
}

fn copy_directory_recursive_inner(from: &Path, to: &Path) {
    let entries = std::fs::read_dir(from).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", from.display());
    });

    for entry in entries.flatten() {
        let source_path = entry.path();
        let destination_path = to.join(entry.file_name());
        let file_type = entry.file_type().unwrap_or_else(|err| {
            panic!(
                "failed to read file type for {}: {err}",
                source_path.display()
            );
        });

        if file_type.is_dir() {
            std::fs::create_dir_all(&destination_path).unwrap_or_else(|err| {
                panic!(
                    "failed to create {}: {err}",
                    destination_path.as_path().display()
                );
            });
            copy_directory_recursive_inner(&source_path, &destination_path);
            continue;
        }

        if file_type.is_file() {
            std::fs::copy(&source_path, &destination_path).unwrap_or_else(|err| {
                panic!(
                    "failed to copy {} to {}: {err}",
                    source_path.display(),
                    destination_path.display()
                );
            });
        }
    }
}

fn is_native_library(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "a" | "so" | "dylib" | "dll" | "lib"))
}

mod ruff_build {
    use std::env;
    use std::process::Command;

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");

        let profile = env::var("PROFILE").expect("PROFILE is required");
        let is_release = profile == "release";

        // Use version-tagged temp directory for persistent cache.
        let cache_dir = super::get_temp_cache_dir(&format!("ruff-{}", super::RUFF_VERSION));
        let cached_ruff_path = cache_dir.join(format!("ruff-{profile}"));

        // Always copy to OUT_DIR for runtime access.
        let out_dir = env::var("OUT_DIR").expect("OUT_DIR is required");
        let ruff_binary_path = format!("{out_dir}/ruff");

        if cached_ruff_path.exists() {
            println!(
                "cargo:warning=Using cached ruff binary from {:?}",
                cached_ruff_path
            );

            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("failed to copy cached ruff binary");
        } else {
            println!("cargo:warning=Building ruff binary from source...");

            let build_dir = cache_dir.join("ruff-src");
            if build_dir.exists() {
                std::fs::remove_dir_all(&build_dir).ok();
            }

            let output = Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--branch",
                    super::RUFF_VERSION,
                    "https://github.com/astral-sh/ruff",
                    build_dir.to_str().expect("valid build dir path"),
                ])
                .output()
                .expect("failed to execute git clone for ruff");

            if !output.status.success() {
                panic!(
                    "failed to clone ruff repository: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }

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
            let status = cmd.status().expect("failed to build ruff");
            assert!(status.success(), "failed to build ruff binary");

            let target_subdir = super::profile_target_subdir(&profile);
            std::fs::copy(
                build_dir.join(format!("target/{target_subdir}/ruff")),
                &cached_ruff_path,
            )
            .expect("failed to cache ruff binary");

            std::fs::copy(&cached_ruff_path, &ruff_binary_path)
                .expect("failed to copy ruff binary to OUT_DIR");

            std::fs::remove_dir_all(&build_dir).ok();
        }

        println!("cargo:rustc-env=RUFF_BINARY_PATH={ruff_binary_path}");
    }
}

mod python_runtime_build {
    use std::path::PathBuf;
    use std::process::Command;

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_py_dir = manifest_dir.join("../peppylib-py");
        let so_path = peppylib_py_dir.join("peppylib/_peppylib.abi3.so");

        // Rerun when peppylib-py Rust source or Cargo.toml changes.
        println!("cargo:rerun-if-changed=../peppylib-py/src/");
        println!("cargo:rerun-if-changed=../peppylib-py/Cargo.toml");

        // Use a separate CARGO_TARGET_DIR so maturin's inner `cargo build`
        // does not deadlock on the workspace build lock held by the outer cargo.
        let cache_dir = super::get_temp_cache_dir("peppylib-py");
        let target_dir = cache_dir.join("target");

        let profile = std::env::var("PROFILE").expect("PROFILE is required");
        let pixi_task = if profile == "release" {
            "release"
        } else {
            "dev"
        };

        println!("cargo:warning=Building peppylib-py native extension via pixi ({pixi_task})…");

        // Serialize concurrent pixi invocations to avoid "Text file busy" races.
        let lock_path = peppylib_py_dir.join(".pixi/.build.lock");
        let _pixi_lock = super::acquire_build_lock(&lock_path);

        let output = Command::new("pixi")
            .args(["run", "-e", "default", pixi_task])
            .current_dir(&peppylib_py_dir)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .output()
            .expect("failed to run `pixi run` for peppylib-py");

        if !output.status.success() {
            panic!(
                "pixi run {pixi_task} failed for peppylib-py:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        assert!(
            so_path.exists(),
            "expected _peppylib.abi3.so at {:?} after pixi run {pixi_task}, but not found",
            so_path,
        );

        // Emit an env var that changes when the .so is rebuilt. This forces
        // cargo to recompile the generator crate so rust_embed re-embeds the
        // fresh native extension.
        let mtime = std::fs::metadata(&so_path)
            .and_then(|meta| meta.modified())
            .expect("failed to read .so metadata")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("mtime should be after UNIX_EPOCH")
            .as_secs();
        println!("cargo:rustc-env=PEPPYLIB_SO_MTIME={mtime}");
    }
}

mod rust_runtime_build {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::path::PathBuf;
    use std::process::Command;

    use serde::Deserialize;

    const BUILD_ARTIFACT_EXTENSIONS: [&str; 7] =
        ["rlib", "rmeta", "so", "dylib", "dll", "a", "lib"];
    const BUILD_WATCHED_PATHS: [&str; 4] = ["Cargo.toml", "build.rs", "src", "schemas"];

    #[derive(Debug, Deserialize)]
    struct CargoMetadata {
        packages: Vec<MetadataPackage>,
        resolve: Option<MetadataResolve>,
    }

    #[derive(Debug, Deserialize)]
    struct MetadataPackage {
        id: String,
        source: Option<String>,
        manifest_path: String,
    }

    #[derive(Debug, Deserialize)]
    struct MetadataResolve {
        nodes: Vec<MetadataNode>,
    }

    #[derive(Debug, Deserialize)]
    struct MetadataNode {
        id: String,
        dependencies: Vec<String>,
    }

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let peppylib_manifest = manifest_dir.join("../peppylib/Cargo.toml");
        emit_rerun_for_runtime_packages(&peppylib_manifest);

        let cache_dir = super::get_temp_cache_dir("peppylib-rust-runtime");
        let target_dir = cache_dir.join("target");
        let lock_path = cache_dir.join(".build.lock");
        let _build_lock = super::acquire_build_lock(&lock_path);

        let profile = std::env::var("PROFILE").expect("PROFILE is required");
        let profile_dir = super::profile_target_subdir(&profile);
        let target_triple = std::env::var("TARGET").expect("TARGET is required");
        let rustc_fingerprint = rustc_fingerprint();

        println!("cargo:warning=Building precompiled Rust runtime artifacts ({profile})…");

        let mut cargo = Command::new("cargo");
        cargo
            .args(["build", "--manifest-path"])
            .arg(&peppylib_manifest)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env_remove("RUSTC")
            .env_remove("RUSTDOC")
            .current_dir(&manifest_dir);
        if profile == "release" {
            cargo.arg("--release");
        }

        let output = cargo
            .output()
            .expect("failed to run cargo build for precompiled Rust runtime");
        if !output.status.success() {
            panic!(
                "cargo build failed for precompiled Rust runtime:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        let deps_source_dir = target_dir.join(profile_dir).join("deps");
        let build_source_dir = target_dir.join(profile_dir).join("build");

        let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is required"));
        let bundle_dir = out_dir.join("precompiled-rust-runtime");
        let bundle_deps_dir = bundle_dir.join("deps");
        let bundle_build_dir = bundle_dir.join("build");

        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir).expect("failed to clear previous runtime bundle");
        }
        std::fs::create_dir_all(&bundle_deps_dir).expect("failed to create bundle deps directory");
        std::fs::create_dir_all(&bundle_build_dir)
            .expect("failed to create bundle build directory");

        let deps_entries = std::fs::read_dir(&deps_source_dir).unwrap_or_else(|err| {
            panic!(
                "failed to read precompiled deps directory {}: {err}",
                deps_source_dir.display()
            );
        });
        for entry in deps_entries.flatten() {
            let source = entry.path();
            if !source.is_file() {
                continue;
            }

            let Some(ext) = source.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if !BUILD_ARTIFACT_EXTENSIONS.contains(&ext) {
                continue;
            }

            let destination = bundle_deps_dir.join(entry.file_name());
            std::fs::copy(&source, &destination).unwrap_or_else(|err| {
                panic!(
                    "failed to copy {} to {}: {err}",
                    source.display(),
                    destination.display()
                );
            });
        }

        if build_source_dir.exists() {
            let build_entries = std::fs::read_dir(&build_source_dir).unwrap_or_else(|err| {
                panic!(
                    "failed to read precompiled build directory {}: {err}",
                    build_source_dir.display()
                );
            });

            for entry in build_entries.flatten() {
                let source_dir = entry.path();
                if !source_dir.is_dir() {
                    continue;
                }
                let source_out_dir = source_dir.join("out");
                if !source_out_dir.exists() {
                    continue;
                }
                if !contains_native_library(&source_out_dir) {
                    continue;
                }

                let destination_out_dir = bundle_build_dir.join(entry.file_name()).join("out");
                super::copy_directory_recursive(&source_out_dir, &destination_out_dir);
            }
        }

        let peppylib_rlib_exists = std::fs::read_dir(&bundle_deps_dir)
            .expect("failed to read bundled deps directory")
            .flatten()
            .map(|entry| entry.path())
            .any(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext == "rlib")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("libpeppylib-"))
            });
        assert!(
            peppylib_rlib_exists,
            "missing libpeppylib-*.rlib in bundled runtime deps at {}",
            bundle_deps_dir.display()
        );

        println!("cargo:rustc-env=PEPPY_RUST_PRECOMPILED_TARGET={target_triple}");
        println!("cargo:rustc-env=PEPPY_RUST_PRECOMPILED_PROFILE_DIR={profile_dir}");
        println!("cargo:rustc-env=PEPPY_RUST_PRECOMPILED_RUSTC={rustc_fingerprint}");
        println!(
            "cargo:rustc-env=PEPPY_RUST_PRECOMPILED_BUNDLE_DIR={}",
            bundle_dir.display()
        );
    }

    fn rustc_fingerprint() -> String {
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
        let output = Command::new(&rustc)
            .arg("-vV")
            .output()
            .unwrap_or_else(|err| panic!("failed to run `{rustc} -vV`: {err}"));
        if !output.status.success() {
            panic!(
                "`{rustc} -vV` failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        let version_info = String::from_utf8(output.stdout)
            .unwrap_or_else(|err| panic!("invalid UTF-8 in `{rustc} -vV` output: {err}"));

        let mut release = None;
        let mut host = None;
        let mut commit_hash = None;

        for line in version_info.lines() {
            if let Some(value) = line.strip_prefix("release: ") {
                release = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("host: ") {
                host = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("commit-hash: ") {
                commit_hash = Some(value.trim().to_owned());
            }
        }

        format!(
            "{}-{}-{}",
            sanitize_path_component(release.as_deref().unwrap_or("unknown")),
            sanitize_path_component(host.as_deref().unwrap_or("unknown")),
            sanitize_path_component(commit_hash.as_deref().unwrap_or("unknown")),
        )
    }

    fn sanitize_path_component(value: &str) -> String {
        let sanitized: String = value
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
            .collect();

        if sanitized.is_empty() {
            "unknown".to_owned()
        } else {
            sanitized
        }
    }

    fn emit_rerun_for_runtime_packages(peppylib_manifest: &std::path::Path) {
        let metadata = read_cargo_metadata(peppylib_manifest);
        let root_package_id = resolve_root_package_id(&metadata, peppylib_manifest);
        let reachable_ids = collect_reachable_package_ids(&metadata, &root_package_id);
        let package_dirs = collect_local_package_dirs(&metadata, &reachable_ids);

        for package_dir in package_dirs {
            for watched_entry in BUILD_WATCHED_PATHS {
                let watched_path = package_dir.join(watched_entry);
                if watched_path.exists() {
                    println!("cargo:rerun-if-changed={}", watched_path.display());
                }
            }
        }
    }

    fn read_cargo_metadata(peppylib_manifest: &std::path::Path) -> CargoMetadata {
        let mut metadata_cmd = Command::new("cargo");
        metadata_cmd
            .args(["metadata", "--format-version", "1", "--manifest-path"])
            .arg(peppylib_manifest)
            .arg("--locked")
            .env_remove("RUSTC")
            .env_remove("RUSTDOC");

        if std::env::var("CARGO_NET_OFFLINE")
            .ok()
            .is_some_and(|value| value == "true")
        {
            metadata_cmd.arg("--offline");
        }

        let output = metadata_cmd
            .output()
            .expect("failed to run cargo metadata for precompiled runtime watching");
        if !output.status.success() {
            panic!(
                "cargo metadata failed for {}:\nstdout:\n{}\nstderr:\n{}",
                peppylib_manifest.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        serde_json::from_slice(&output.stdout)
            .expect("failed to parse cargo metadata JSON for precompiled runtime watching")
    }

    fn resolve_root_package_id(
        metadata: &CargoMetadata,
        peppylib_manifest: &std::path::Path,
    ) -> String {
        let target_manifest = std::fs::canonicalize(peppylib_manifest).unwrap_or_else(|_| {
            panic!(
                "failed to canonicalize peppylib manifest {}",
                peppylib_manifest.display()
            )
        });

        metadata
            .packages
            .iter()
            .find_map(|package| {
                let manifest_path = std::path::Path::new(&package.manifest_path);
                let canonical_manifest = std::fs::canonicalize(manifest_path).ok()?;
                (canonical_manifest == target_manifest).then_some(package.id.clone())
            })
            .unwrap_or_else(|| {
                panic!(
                    "failed to resolve peppylib package id from cargo metadata for {}",
                    peppylib_manifest.display()
                )
            })
    }

    fn collect_reachable_package_ids(
        metadata: &CargoMetadata,
        root_package_id: &str,
    ) -> HashSet<String> {
        let mut reachable = HashSet::new();
        let mut pending = VecDeque::from([root_package_id.to_owned()]);

        let dependency_graph: HashMap<&str, Vec<&str>> = metadata
            .resolve
            .as_ref()
            .map(|resolve| {
                resolve
                    .nodes
                    .iter()
                    .map(|node| {
                        (
                            node.id.as_str(),
                            node.dependencies.iter().map(String::as_str).collect(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        while let Some(package_id) = pending.pop_front() {
            if !reachable.insert(package_id.clone()) {
                continue;
            }

            if let Some(dependencies) = dependency_graph.get(package_id.as_str()) {
                for dependency in dependencies {
                    pending.push_back((*dependency).to_owned());
                }
            }
        }

        reachable
    }

    fn collect_local_package_dirs(
        metadata: &CargoMetadata,
        reachable_ids: &HashSet<String>,
    ) -> Vec<PathBuf> {
        let mut package_dirs: Vec<PathBuf> = metadata
            .packages
            .iter()
            .filter(|package| package.source.is_none() && reachable_ids.contains(&package.id))
            .filter_map(|package| {
                std::path::Path::new(&package.manifest_path)
                    .parent()
                    .map(PathBuf::from)
            })
            .collect();

        package_dirs.sort();
        package_dirs.dedup();
        package_dirs
    }

    fn contains_native_library(dir: &std::path::Path) -> bool {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return false,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                if contains_native_library(&path) {
                    return true;
                }
                continue;
            }

            if file_type.is_file() && super::is_native_library(&path) {
                return true;
            }
        }

        false
    }
}

fn main() {
    ruff_build::run();
    python_runtime_build::run();
    rust_runtime_build::run();
}
