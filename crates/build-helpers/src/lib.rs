use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns a cache directory under `~/.peppy/tmp/{suffix}`, creating it if needed.
pub fn cache_dir(suffix: &str) -> PathBuf {
    let user_home = std::env::var("HOME").expect("HOME environment variable not set");
    let cache_dir = PathBuf::from(user_home).join(".peppy/tmp").join(suffix);

    if !cache_dir.exists() {
        std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
    }

    cache_dir
}

/// Returns the Rust target triple for the current build from cargo env vars.
pub fn build_target_triple() -> String {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    let env_abi = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (os.as_str(), arch.as_str(), env_abi.as_str()) {
        ("macos", "aarch64", _) => "aarch64-apple-darwin".to_string(),
        ("macos", "x86_64", _) => "x86_64-apple-darwin".to_string(),
        ("linux", "x86_64", "gnu") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64", "gnu") => "aarch64-unknown-linux-gnu".to_string(),
        ("linux", "riscv64", "gnu") => "riscv64gc-unknown-linux-gnu".to_string(),
        _ => format!("{arch}-unknown-{os}-{env_abi}"),
    }
}

/// Runs a command and prints a cargo warning on failure. Returns `true` on success.
pub fn run_command(command: &mut Command, description: &str) -> bool {
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("cargo:warning=Failed to {description} (exit status: {status})");
            false
        }
        Err(err) => {
            println!("cargo:warning=Failed to {description}: {err}");
            false
        }
    }
}

/// Computes the SHA-256 hash of a file using the `sha2` crate. Returns `None` on I/O error.
pub fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            println!(
                "cargo:warning=Failed to open {:?} for SHA-256 verification: {}",
                path, e
            );
            return None;
        }
    };

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let n = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(e) => {
                println!(
                    "cargo:warning=Failed to read {:?} for SHA-256 verification: {}",
                    path, e
                );
                return None;
            }
        };
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    Some(format!("{:x}", hasher.finalize()))
}

/// Verifies the SHA-256 hash of a file against an expected value.
/// Returns `true` if the hash matches.
pub fn verify_sha256(path: &Path, expected: &str, label: &str) -> bool {
    let Some(actual) = sha256_file(path) else {
        return false;
    };

    if actual.eq_ignore_ascii_case(expected) {
        true
    } else {
        println!(
            "cargo:warning={} SHA-256 mismatch for {:?}: expected {}, got {}",
            label, path, expected, actual
        );
        false
    }
}

/// Embed the `PEPPY_GIT_TAG` environment variable into the binary at compile time.
///
/// If `PEPPY_GIT_TAG` is set and non-empty (by build_release.sh), emits a
/// `cargo:rustc-env` directive so the crate can read it via `env!()`.
/// Also registers `cargo:rerun-if-env-changed` so cargo rebuilds when the
/// variable changes.
pub fn embed_git_tag() {
    if let Ok(git_tag) = std::env::var("PEPPY_GIT_TAG") {
        if !git_tag.is_empty() {
            println!("cargo:rustc-env=PEPPY_GIT_TAG={git_tag}");
        }
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

    let crate_spec = format!("{name}@{version}");
    let mut cmd = Command::new("cargo");
    cmd.args([
        "install",
        &crate_spec,
        "--target",
        target,
        "--root",
        install_root.to_str().unwrap(),
    ])
    .env("CARGO_TARGET_DIR", &cargo_target_dir);

    let output = cmd.output();
    match &output {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            println!(
                "cargo:warning=cargo install {crate_spec} failed (exit: {}): {}",
                o.status, stderr
            );
            std::fs::remove_dir_all(&install_root).ok();
            return None;
        }
        Err(e) => {
            println!("cargo:warning=Failed to run cargo install: {e}");
            std::fs::remove_dir_all(&install_root).ok();
            return None;
        }
    }

    let built_binary = install_root.join("bin").join(name);
    if !built_binary.exists() {
        println!(
            "cargo:warning=cargo install succeeded but binary not found at {:?}",
            built_binary
        );
        std::fs::remove_dir_all(&install_root).ok();
        return None;
    }

    if let Err(e) = std::fs::copy(&built_binary, &cached_binary) {
        println!("cargo:warning=Failed to cache compiled {name} binary: {e}");
        std::fs::remove_dir_all(&install_root).ok();
        return None;
    }

    // Clean up the install temp directory (keep cargo-build-{name} for incremental builds)
    std::fs::remove_dir_all(&install_root).ok();

    println!(
        "cargo:warning=Successfully compiled and cached {name} {version} for {target}"
    );
    Some(cached_binary)
}
