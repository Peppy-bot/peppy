//! Cargo/build-environment helpers: target triples, env embedding, and
//! locating or compiling tool binaries.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::command::run_command_streaming;
use crate::fs::CleanupDir;

/// Returns the Rust target triple for the current build from cargo env vars.
///
/// Must be called from a build script. It reads `CARGO_CFG_TARGET_ARCH`,
/// `CARGO_CFG_TARGET_OS`, and `CARGO_CFG_TARGET_ENV`, which cargo only sets
/// while running `build.rs`. The arch and OS reads `unwrap()` on purpose: their
/// absence means the function was called outside that context, which is a
/// programming error rather than a recoverable runtime condition.
pub fn build_target_triple() -> String {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let env_abi = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (os.as_str(), arch.as_str(), env_abi.as_str()) {
        ("macos", "aarch64", _) => "aarch64-apple-darwin".to_string(),
        ("linux", "x86_64", "gnu") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64", "gnu") => "aarch64-unknown-linux-gnu".to_string(),
        _ => format!("{arch}-unknown-{os}-{env_abi}"),
    }
}

/// Embed the `PEPPY_GIT_TAG` environment variable into the binary at compile time.
///
/// If `PEPPY_GIT_TAG` is set and non-empty (by build_release.sh), emits a
/// `cargo:rustc-env` directive so the crate can read it via `env!()`.
/// Also registers `cargo:rerun-if-env-changed` so cargo rebuilds when the
/// variable changes.
pub fn embed_git_tag() {
    if let Ok(git_tag) = std::env::var("PEPPY_GIT_TAG")
        && !git_tag.is_empty()
    {
        println!("cargo:rustc-env=PEPPY_GIT_TAG={git_tag}");
    }
    println!("cargo:rerun-if-env-changed=PEPPY_GIT_TAG");
}

/// Find the bundled capnp binary for the current host platform in `tools_dir`.
///
/// Returns `Some(path)` if a binary matching the host OS/arch exists,
/// `None` otherwise. The `tools_dir` should point to the directory containing
/// platform-specific capnp binaries (e.g. `crates/config-internal/tools/`).
pub fn find_bundled_capnp(tools_dir: &Path) -> Option<PathBuf> {
    let binary_name = host_capnp_binary_name();
    let binary_path = tools_dir.join(binary_name);
    if binary_path.exists() {
        Some(binary_path)
    } else {
        None
    }
}

fn host_capnp_binary_name() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "capnp_linux_x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "capnp_linux_aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "capnp_macos_aarch64"
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    {
        "capnp_unsupported"
    }
}

/// Compile a Rust binary from crates.io using `cargo install` with cross-compilation support.
///
/// Returns `Some(path)` to the cached binary on success, `None` on failure.
/// Uses a separate `CARGO_TARGET_DIR` to avoid lock conflicts with the outer cargo build.
pub fn cargo_install_binary(
    name: &str,
    version: &str,
    target: &str,
    cache_dir: &Path,
) -> Option<PathBuf> {
    let cached_binary = cache_dir.join(format!("{name}-{version}-{target}"));

    if cached_binary.exists() {
        println!(
            "cargo:warning=Using cached {name} binary from {:?}",
            cached_binary
        );
        return Some(cached_binary);
    }

    println!(
        "cargo:warning=Compiling {name} {version} from source for {target} (this may take several minutes)..."
    );

    let install_root = cache_dir.join(format!("{name}-install-tmp"));
    let cargo_target_dir = cache_dir.join(format!("cargo-build-{name}"));

    // Clean up any previous partial install
    if install_root.exists() {
        std::fs::remove_dir_all(&install_root).ok();
    }
    std::fs::create_dir_all(&install_root).ok();
    std::fs::create_dir_all(&cargo_target_dir).ok();

    // Guards ensure temp directories are cleaned up on all exit paths.
    let _install_guard = CleanupDir(install_root.clone());
    let _target_guard = CleanupDir(cargo_target_dir.clone());

    let crate_spec = format!("{name}@{version}");
    let Some(install_root_str) = install_root.to_str() else {
        println!("cargo:warning=Install root path is not valid UTF-8: {install_root:?}");
        return None;
    };
    let mut cmd = Command::new("cargo");
    cmd.args([
        "install",
        &crate_spec,
        "--target",
        target,
        "--root",
        install_root_str,
    ])
    .env("CARGO_TARGET_DIR", &cargo_target_dir);

    let label = format!("cargo-install-{name}");
    let output = run_command_streaming(&mut cmd, &label);
    if !output.success {
        return None;
    }

    let built_binary = install_root.join("bin").join(name);
    if !built_binary.exists() {
        println!(
            "cargo:warning=cargo install succeeded but binary not found at {:?}",
            built_binary
        );
        return None;
    }

    if let Err(e) = std::fs::copy(&built_binary, &cached_binary) {
        println!("cargo:warning=Failed to cache compiled {name} binary: {e}");
        return None;
    }

    println!("cargo:warning=Successfully compiled and cached {name} {version} for {target}");
    Some(cached_binary)
}
