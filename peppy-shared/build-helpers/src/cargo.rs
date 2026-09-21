//! Cargo/build-environment helpers: target triples, env embedding, and
//! locating or compiling tool binaries.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::command::run_command_streaming;
use crate::fs::{CleanupDir, acquire_file_lock};

/// Returns the Rust target triple for the current build.
///
/// Must be called from a build script. It reads the `TARGET` environment
/// variable, which cargo only sets while running `build.rs`. The read
/// `expect()`s on purpose: the variable's absence means the function was
/// called outside that context, which is a programming error rather than a
/// recoverable runtime condition.
pub fn build_target_triple() -> String {
    std::env::var("TARGET")
        .expect("TARGET not set; build_target_triple must be called from a build script")
}

/// Embed the `PEPPY_GIT_TAG` environment variable into the binary at compile time.
///
/// If `PEPPY_GIT_TAG` is set and non-empty (by build_release.sh), emits a
/// `cargo:rustc-env` directive so the crate can read it via `env!()`.
/// Also registers `cargo:rerun-if-env-changed` so cargo rebuilds when the
/// variable changes.
pub fn embed_git_tag() {
    let tag = std::env::var("PEPPY_GIT_TAG").ok();
    for line in git_tag_directives(tag.as_deref()) {
        println!("{line}");
    }
}

/// Cargo directives emitted by [`embed_git_tag`], in emission order.
fn git_tag_directives(tag: Option<&str>) -> Vec<String> {
    let mut directives = Vec::new();
    if let Some(tag) = tag
        && !tag.is_empty()
    {
        directives.push(format!("cargo:rustc-env=PEPPY_GIT_TAG={tag}"));
    }
    directives.push("cargo:rerun-if-env-changed=PEPPY_GIT_TAG".to_string());
    directives
}

/// Platforms for which peppy ships a Cap'n Proto compiler.
///
/// Private on purpose: build scripts never pick a platform themselves. They
/// call [`host_capnp_for_execution`] to run the compiler and
/// [`bundled_capnp_for_embedding`] to ship it inside the artifact, and each
/// of those resolves the platform the only way that is correct for its use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapnpPlatform {
    LinuxX86_64,
    LinuxAarch64,
    MacosAarch64,
}

impl CapnpPlatform {
    /// Returns the platform on which the build script itself is running.
    fn current_host() -> Option<Self> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            return Some(Self::LinuxX86_64);
        }
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            return Some(Self::LinuxAarch64);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            return Some(Self::MacosAarch64);
        }
        #[allow(unreachable_code)]
        None
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "capnp_linux_x86_64",
            Self::LinuxAarch64 => "capnp_linux_aarch64",
            Self::MacosAarch64 => "capnp_macos_aarch64",
        }
    }
}

/// Error returned when a Rust target triple has no bundled Cap'n Proto binary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedCapnpTarget {
    target: String,
}

impl UnsupportedCapnpTarget {
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl fmt::Display for UnsupportedCapnpTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported Cap'n Proto target {:?}; supported targets: \
             x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, \
             aarch64-apple-darwin",
            self.target
        )
    }
}

impl std::error::Error for UnsupportedCapnpTarget {}

impl TryFrom<&str> for CapnpPlatform {
    type Error = UnsupportedCapnpTarget;

    fn try_from(target: &str) -> Result<Self, Self::Error> {
        match target {
            "x86_64-unknown-linux-gnu" => Ok(Self::LinuxX86_64),
            "aarch64-unknown-linux-gnu" => Ok(Self::LinuxAarch64),
            "aarch64-apple-darwin" => Ok(Self::MacosAarch64),
            _ => Err(UnsupportedCapnpTarget {
                target: target.to_string(),
            }),
        }
    }
}

/// The tools dir holding the bundled Cap'n Proto compilers:
/// `peppy-config-model/tools` inside [`peppy_shared_dir`].
fn bundled_tools_dir() -> PathBuf {
    peppy_shared_dir().join(CONFIG_MODEL_CRATE).join("tools")
}

/// Path to the bundled Cap'n Proto compiler that runs on the machine
/// executing the current build script.
///
/// Schema compilation always goes through this function: the generated Rust
/// source is identical for every cargo target, so the compiler is picked by
/// build host, never by target triple. On a cross-compile the target's
/// binary cannot execute on the build machine; [`bundled_capnp_for_embedding`]
/// serves the one consumer that ships the target's binary inside the
/// artifact instead of running it.
///
/// Emits a `cargo:rerun-if-changed` directive for the binary so a compiler
/// update triggers fresh code generation. Panics when the host platform has no
/// bundled compiler or [`bundled_tools_dir`] does not hold it, because a build
/// script cannot recover from either.
pub fn host_capnp_for_execution() -> PathBuf {
    let platform = CapnpPlatform::current_host().unwrap_or_else(|| {
        panic!(
            "no bundled capnp compiler runs on build host {}/{}; supported hosts: \
             linux/x86_64, linux/aarch64, macos/aarch64",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    });
    let binary_path = existing_bundled_capnp(platform);
    println!("cargo:rerun-if-changed={}", binary_path.display());
    binary_path
}

/// Path to the bundled Cap'n Proto compiler built for `target`, for embedding
/// into the compiled artifact.
///
/// The returned binary is payload, not a tool: on a cross-compile it does not
/// run on the build machine, so it must never be executed by a build script.
/// Build scripts that run the compiler use [`host_capnp_for_execution`].
/// Returns [`UnsupportedCapnpTarget`] when `target` has no bundled compiler,
/// and panics when the target is supported but the binary file is absent from
/// [`bundled_tools_dir`], because that means the checkout itself is broken.
pub fn bundled_capnp_for_embedding(target: &str) -> Result<PathBuf, UnsupportedCapnpTarget> {
    let platform = CapnpPlatform::try_from(target)?;
    Ok(existing_bundled_capnp(platform))
}

/// Path to the bundled compiler for `platform`, which must exist: every
/// supported platform ships its binary in [`bundled_tools_dir`], so a missing
/// file means the tree itself is broken.
fn existing_bundled_capnp(platform: CapnpPlatform) -> PathBuf {
    let binary_path = bundled_tools_dir().join(platform.binary_name());
    if !binary_path.exists() {
        panic!(
            "bundled capnp binary missing at {}; add it to \
             peppy-shared/peppy-config-model/tools/",
            binary_path.display()
        );
    }
    binary_path
}

/// Crate whose presence marks a directory as `peppy-shared`: every layout of
/// the shared crates keeps it, because it carries the bundled tools.
const CONFIG_MODEL_CRATE: &str = "peppy-config-model";

/// Where the `peppy` workspace keeps `peppy-shared`, relative to its root.
const PEPPY_SHARED_IN_WORKSPACE: &str = "peppy-shared";

/// Locate the `peppy-shared` directory: the one holding every shared crate
/// (`peppylib-rs`, `peppy-config-model`, `core-node-api`,
/// `peppy-messaging-interface`, `peppylib-py`, ...).
///
/// The directory is found when the calling build script or test runs, by
/// walking up from the `CARGO_MANIFEST_DIR` cargo sets for that run (see
/// [`find_peppy_shared_dir`]). It is never baked in at compile time: cargo
/// reuses a compiled `build-helpers` across every checkout that shares a
/// target directory, so a compile-time path would name whichever checkout
/// compiled the crate instead of the one being built.
///
/// Consumers such as `generator`'s build script use this to find the shared
/// crate source trees they embed, giving one source of truth instead of a
/// relative path duplicated at every call site. Panics when
/// `CARGO_MANIFEST_DIR` is unset or no `peppy-shared` directory is reachable
/// from it, because a build script cannot recover from either.
pub fn peppy_shared_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect(
        "CARGO_MANIFEST_DIR not set; peppy_shared_dir must be called from a build script or test",
    ));
    // Deployed flat-cache layouts reach crates through symlinks, so the walk
    // starts from the real location of the manifest dir.
    let manifest_dir = manifest_dir.canonicalize().unwrap_or(manifest_dir);
    find_peppy_shared_dir(&manifest_dir).unwrap_or_else(|| {
        panic!(
            "no peppy-shared directory reachable from {}: no ancestor holds \
             {CONFIG_MODEL_CRATE} or {PEPPY_SHARED_IN_WORKSPACE}/{CONFIG_MODEL_CRATE}",
            manifest_dir.display()
        )
    })
}

/// Finds the `peppy-shared` directory nearest to `start`, walking up through
/// its ancestors. Each ancestor is checked two ways:
///   - It holds the shared crates itself. This covers a crate inside
///     `peppy-shared`, whether in a working checkout or in the whole-repository
///     checkout cargo makes for a git dependency (as `platform-backend` pulls
///     `core-node-api`), and deployed flat-cache layouts that copy each crate
///     next to `peppy-config-model` under a directory of any name.
///   - It is the `peppy` workspace root, which keeps the tree at
///     `peppy-shared`. This covers the workspace crates outside the tree, such
///     as `crates/encoding-internal`.
fn find_peppy_shared_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|ancestor| {
        [
            ancestor.to_path_buf(),
            ancestor.join(PEPPY_SHARED_IN_WORKSPACE),
        ]
        .into_iter()
        .find(|candidate| candidate.join(CONFIG_MODEL_CRATE).is_dir())
    })
}

/// Compile a Rust binary from crates.io using `cargo install` with cross-compilation support.
///
/// Returns `Some(path)` to the cached binary on success, `None` on failure.
/// Uses a separate `CARGO_TARGET_DIR` to avoid lock conflicts with the outer
/// cargo build. Concurrent installs of the same tool sharing a `cache_dir`
/// (for example, two worktrees building at once) are serialized with a file
/// lock — acquisition blocks until the lock is free and panics if the lock
/// cannot be taken — and the cached binary is published with an atomic
/// rename so a concurrent reader can never observe a partially written file.
pub fn cargo_install_binary(
    name: &str,
    version: &str,
    target: &str,
    cache_dir: &Path,
) -> Option<PathBuf> {
    // Cargo sets $CARGO to the exact cargo that launched this build script. Run
    // the nested install with that same binary so it matches the outer build's
    // toolchain instead of whatever cargo happens to come first on PATH.
    let cargo_program = std::env::var_os("CARGO").map(PathBuf::from);
    let cargo_program = cargo_program.as_deref().unwrap_or(Path::new("cargo"));
    cargo_install_binary_with(cargo_program, name, version, target, cache_dir)
}

/// Implementation of [`cargo_install_binary`] with the cargo executable made
/// explicit, so tests can substitute a fixture script.
fn cargo_install_binary_with(
    cargo_program: &Path,
    name: &str,
    version: &str,
    target: &str,
    cache_dir: &Path,
) -> Option<PathBuf> {
    fn use_cached(name: &str, cached_binary: PathBuf) -> Option<PathBuf> {
        crate::progress!("Using cached {name} binary from {cached_binary:?}");
        Some(cached_binary)
    }

    let cached_binary = cache_dir.join(format!("{name}-{version}-{target}"));

    if cached_binary.exists() {
        return use_cached(name, cached_binary);
    }

    // Serialize concurrent installs sharing this cache dir, then re-check:
    // another process may have populated the cache while we waited. The lock
    // is keyed by name alone because the install/build temp dirs below are
    // shared by all versions and targets of the tool.
    let _lock = acquire_file_lock(&cache_dir.join(format!("{name}.lock")));
    if cached_binary.exists() {
        return use_cached(name, cached_binary);
    }

    crate::progress!(
        "Compiling {name} {version} from source for {target} (this may take several minutes)..."
    );

    let install_root = cache_dir.join(format!("{name}-install-tmp"));
    let cargo_target_dir = cache_dir.join(format!("cargo-build-{name}"));

    // Clean up any previous partial install
    std::fs::remove_dir_all(&install_root).ok();
    std::fs::create_dir_all(&install_root).ok();
    std::fs::create_dir_all(&cargo_target_dir).ok();

    // Guards ensure temp directories are cleaned up on all exit paths.
    let _install_guard = CleanupDir(install_root.clone());
    let _target_guard = CleanupDir(cargo_target_dir.clone());

    let crate_spec = format!("{name}@{version}");
    let mut cmd = Command::new(cargo_program);
    cmd.args(["install", &crate_spec, "--target", target, "--root"])
        .arg(&install_root)
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

    // Publish atomically: stage next to the cache key, then rename onto it,
    // so the lock-free fast path above never observes a torn binary. The
    // fixed staging name cannot collide — staging only happens under the
    // lock — and a leftover from a killed build is truncated by the copy.
    let staged = cache_dir.join(format!("{name}-{version}-{target}.tmp"));
    let published = std::fs::copy(&built_binary, &staged)
        .and_then(|_| std::fs::rename(&staged, &cached_binary));
    if let Err(e) = published {
        std::fs::remove_file(&staged).ok();
        println!("cargo:warning=Failed to cache compiled {name} binary: {e}");
        return None;
    }

    crate::progress!("Successfully compiled and cached {name} {version} for {target}");
    Some(cached_binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RERUN_DIRECTIVE: &str = "cargo:rerun-if-env-changed=PEPPY_GIT_TAG";
    const SUPPORTED_CAPNP_PLATFORMS: [CapnpPlatform; 3] = [
        CapnpPlatform::LinuxX86_64,
        CapnpPlatform::LinuxAarch64,
        CapnpPlatform::MacosAarch64,
    ];

    #[test]
    fn git_tag_directives_emits_rustc_env_then_rerun_for_nonempty_tag() {
        assert_eq!(
            git_tag_directives(Some("v1.2.3")),
            ["cargo:rustc-env=PEPPY_GIT_TAG=v1.2.3", RERUN_DIRECTIVE]
        );
    }

    #[test]
    fn git_tag_directives_emits_only_rerun_when_tag_unset() {
        assert_eq!(git_tag_directives(None), [RERUN_DIRECTIVE]);
    }

    #[test]
    fn git_tag_directives_emits_only_rerun_when_tag_empty() {
        assert_eq!(git_tag_directives(Some("")), [RERUN_DIRECTIVE]);
    }

    /// Creates every directory of `relative_dirs` under `root`.
    fn create_dirs(root: &Path, relative_dirs: &[&str]) {
        for relative_dir in relative_dirs {
            std::fs::create_dir_all(root.join(relative_dir)).expect("create layout dir");
        }
    }

    /// Lays out a `peppy` workspace under `root`: the shared tree plus one
    /// workspace crate outside it.
    fn create_workspace(root: &Path) {
        create_dirs(
            root,
            &[
                "crates/encoding-internal",
                "peppy-shared/peppy-config-model",
                "peppy-shared/core-node-api",
            ],
        );
    }

    #[test]
    fn find_peppy_shared_dir_resolves_a_crate_inside_the_tree() {
        let root = tempfile::tempdir().expect("temp dir");
        create_workspace(root.path());
        let shared = root.path().join("peppy-shared");
        assert_eq!(
            find_peppy_shared_dir(&shared.join("core-node-api")),
            Some(shared)
        );
    }

    #[test]
    fn find_peppy_shared_dir_resolves_a_workspace_crate_outside_the_tree() {
        let root = tempfile::tempdir().expect("temp dir");
        create_workspace(root.path());
        assert_eq!(
            find_peppy_shared_dir(&root.path().join("crates/encoding-internal")),
            Some(root.path().join("peppy-shared"))
        );
    }

    #[test]
    fn find_peppy_shared_dir_resolves_a_flat_cache_under_any_dir_name() {
        let root = tempfile::tempdir().expect("temp dir");
        create_dirs(
            root.path(),
            &["libs-cache/peppy-config-model", "libs-cache/core-node-api"],
        );
        assert_eq!(
            find_peppy_shared_dir(&root.path().join("libs-cache/core-node-api")),
            Some(root.path().join("libs-cache"))
        );
    }

    #[test]
    fn find_peppy_shared_dir_keeps_each_checkout_on_its_own_tree() {
        // Two checkouts of the same workspace share one compiled build-helpers
        // when they share a target dir, so the answer must follow the start dir.
        let first = tempfile::tempdir().expect("temp dir");
        let second = tempfile::tempdir().expect("temp dir");
        create_workspace(first.path());
        create_workspace(second.path());
        for checkout in [first.path(), second.path()] {
            assert_eq!(
                find_peppy_shared_dir(&checkout.join("crates/encoding-internal")),
                Some(checkout.join("peppy-shared"))
            );
        }
    }

    #[test]
    fn find_peppy_shared_dir_prefers_the_nearest_tree() {
        let root = tempfile::tempdir().expect("temp dir");
        create_workspace(root.path());
        create_dirs(
            root.path(),
            &["libs-cache/peppy-config-model", "libs-cache/core-node-api"],
        );
        assert_eq!(
            find_peppy_shared_dir(&root.path().join("libs-cache/core-node-api")),
            Some(root.path().join("libs-cache"))
        );
    }

    #[test]
    fn find_peppy_shared_dir_ignores_a_marker_that_is_not_a_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        create_dirs(root.path(), &["libs-cache/core-node-api"]);
        std::fs::write(root.path().join("libs-cache/peppy-config-model"), b"")
            .expect("create marker file");
        assert_eq!(
            find_peppy_shared_dir(&root.path().join("libs-cache/core-node-api")),
            None
        );
    }

    #[test]
    fn find_peppy_shared_dir_returns_none_when_no_tree_is_reachable() {
        let root = tempfile::tempdir().expect("temp dir");
        create_dirs(root.path(), &["crates/encoding-internal"]);
        assert_eq!(
            find_peppy_shared_dir(&root.path().join("crates/encoding-internal")),
            None
        );
    }

    #[test]
    fn host_capnp_for_execution_returns_the_host_platform_binary() {
        let host_platform = CapnpPlatform::current_host().expect("supported build host");
        let path = host_capnp_for_execution();
        assert!(path.exists(), "{} should exist", path.display());
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(host_platform.binary_name())
        );
    }

    #[test]
    fn bundled_capnp_for_embedding_resolves_every_supported_target() {
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
        ] {
            let path = bundled_capnp_for_embedding(target).expect(target);
            assert!(path.exists(), "{} should exist", path.display());
        }
    }

    #[test]
    fn bundled_capnp_for_embedding_rejects_unsupported_target() {
        let error = bundled_capnp_for_embedding("x86_64-pc-windows-msvc").expect_err("windows");
        assert_eq!(error.target(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn capnp_platform_parses_every_supported_target() {
        for (target, expected) in [
            ("x86_64-unknown-linux-gnu", CapnpPlatform::LinuxX86_64),
            ("aarch64-unknown-linux-gnu", CapnpPlatform::LinuxAarch64),
            ("aarch64-apple-darwin", CapnpPlatform::MacosAarch64),
        ] {
            assert_eq!(CapnpPlatform::try_from(target), Ok(expected), "{target}");
        }
    }

    #[test]
    fn capnp_platform_rejects_unsupported_or_malformed_targets() {
        for target in [
            "",
            "x86_64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "arm-unknown-linux-gnueabihf",
            "linux-x86_64",
        ] {
            let error = CapnpPlatform::try_from(target).expect_err(target);
            assert_eq!(error.target(), target);
        }
    }

    #[test]
    fn bundled_tools_dir_holds_every_supported_platform_binary() {
        for platform in SUPPORTED_CAPNP_PLATFORMS {
            assert!(
                bundled_tools_dir().join(platform.binary_name()).exists(),
                "expected a bundled capnp binary for {platform:?}"
            );
        }
    }

    #[test]
    fn bundled_capnp_binaries_are_version_1_5_0() {
        const EXPECTED_VERSION: &[u8] = b"Cap'n Proto version 1.5.0";

        for platform in SUPPORTED_CAPNP_PLATFORMS {
            let path = bundled_tools_dir().join(platform.binary_name());
            let binary = std::fs::read(&path).expect("read bundled capnp binary");
            assert!(
                binary
                    .windows(EXPECTED_VERSION.len())
                    .any(|window| window == EXPECTED_VERSION),
                "{} does not contain the expected Cap'n Proto version",
                path.display()
            );
        }
    }

    #[test]
    fn peppy_shared_dir_contains_sibling_crates() {
        // The locator must point at the real `peppy-shared` dir: the place that
        // holds this crate alongside its siblings. Assert via crates that always
        // exist so consumers (e.g. generator) can rely on joining a sibling name.
        let shared = peppy_shared_dir();
        for sibling in ["build-helpers", "peppy-config-model", "peppylib-rs"] {
            assert!(
                shared.join(sibling).is_dir(),
                "peppy_shared_dir() should contain {sibling}, got {}",
                shared.display()
            );
        }
    }

    #[test]
    fn cargo_install_binary_returns_cached_binary_without_installing() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Pre-populating the `{name}-{version}-{target}` cache key must
        // short-circuit the install; this pins the filename contract that
        // peppy-messaging-interface and generator-internal build scripts rely on. The
        // missing cargo program makes a fast-path regression fail fast
        // instead of invoking the real cargo against the network.
        let cached = dir.path().join("mytool-1.0.0-x86_64-unknown-linux-gnu");
        std::fs::write(&cached, b"cached").expect("pre-populate cache");
        assert_eq!(
            cargo_install_binary_with(
                &dir.path().join("no-such-cargo"),
                "mytool",
                "1.0.0",
                "x86_64-unknown-linux-gnu",
                dir.path()
            ),
            Some(cached)
        );
    }

    /// Writes an executable shell script that stands in for `cargo` so the
    /// install paths can be tested without network access or PATH mutation.
    ///
    /// The script is written by a child shell rather than `std::fs::write`:
    /// a write fd opened in this multithreaded test process leaks into
    /// children forked concurrently by other tests, and exec'ing a file
    /// somebody still holds open for writing fails with ETXTBSY.
    #[cfg(unix)]
    fn write_fake_cargo(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-cargo");
        let status = Command::new("sh")
            .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "sh"])
            .arg(format!("#!/bin/sh\n{body}\n"))
            .arg(&path)
            .status()
            .expect("write fixture script");
        assert!(status.success(), "fixture script write failed");
        path
    }

    #[cfg(unix)]
    fn temp_cache_dir(dir: &Path) -> PathBuf {
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).expect("create cache dir");
        cache
    }

    #[cfg(unix)]
    #[test]
    fn cargo_install_binary_with_caches_built_binary_and_cleans_temp_dirs() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The script fakes a successful `cargo install` by writing
        // bin/<name> under the --root it is given.
        let script = write_fake_cargo(
            dir.path(),
            r#"root=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--root" ]; then root="$2"; shift; fi
  shift
done
mkdir -p "$root/bin"
printf fake-binary > "$root/bin/mytool""#,
        );
        let cache = temp_cache_dir(dir.path());

        let result = cargo_install_binary_with(&script, "mytool", "1.0.0", "test-target", &cache);

        let cached = cache.join("mytool-1.0.0-test-target");
        assert_eq!(result, Some(cached.clone()));
        assert_eq!(std::fs::read(&cached).expect("read cached"), b"fake-binary");
        assert!(!cache.join("mytool-install-tmp").exists());
        assert!(!cache.join("cargo-build-mytool").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cargo_install_binary_with_returns_none_on_install_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = write_fake_cargo(dir.path(), "exit 1");
        let cache = temp_cache_dir(dir.path());

        assert_eq!(
            cargo_install_binary_with(&script, "mytool", "1.0.0", "test-target", &cache),
            None
        );
        assert!(!cache.join("mytool-install-tmp").exists());
        assert!(!cache.join("cargo-build-mytool").exists());
    }
}
