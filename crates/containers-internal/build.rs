mod apptainer_build {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const APPTAINER_VERSION: &str = "1.4.5";
    const LIMA_VERSION: &str = "2.1.0";
    const LIMA_DARWIN_ARM64_ARCHIVE_SHA256: &str =
        "1da852bce2f98b8310fb53e5047e08ff798880ddf9ae4b3161d4de4e73777b34";
    const LIMA_INSTANCE: &str = "peppy";
    const LIMA_TEMPLATE: &str = "template:ubuntu-24.04";
    /// Guest-side installation path for apptainer inside the Lima VM.
    /// Must match the `--prefix` used at build time.
    const GUEST_APPTAINER_DIR: &str = "/tmp/peppy/apptainer";

    // -----------------------------------------------------------------------
    // Cache helpers
    // -----------------------------------------------------------------------

    fn apptainer_cache_sentinel_path(cache_dir: &Path, version: &str) -> PathBuf {
        cache_dir.join(format!(".peppy-version-{}", version))
    }

    /// Remove a directory tree, falling back to `rm -rf` if `std::fs`
    /// fails (e.g. due to root-owned files left by a previous Lima VM build).
    fn force_remove_dir(dir: &Path) {
        if !dir.exists() {
            return;
        }
        if std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        // Fall back to rm -rf which can sometimes succeed where std::fs
        // cannot (e.g. when directory permissions differ).
        let _ = Command::new("rm").args(["-rf"]).arg(dir).status();
        if dir.exists() {
            panic!(
                "Cannot remove stale apptainer cache at {:?} (likely contains root-owned files \
                 from a previous Lima VM build). Please remove it manually:\n  \
                 sudo rm -rf {:?}",
                dir, dir
            );
        }
    }

    /// LIMA_HOME for the build-time VM instance.
    ///
    /// Uses `~/.peppy/lima-build/` instead of the temp dir because macOS temp
    /// directories (`/var/folders/.../T/`) are very long and push Unix socket
    /// paths past the 104-character limit.
    fn get_lima_build_home() -> PathBuf {
        let user_home = env::var("HOME").expect("HOME environment variable not set");
        let home = PathBuf::from(user_home).join(".peppy/lima-build");
        std::fs::create_dir_all(&home).expect("Failed to create lima build data directory");
        home
    }

    // -----------------------------------------------------------------------
    // Lima download and extraction (macOS only)
    // -----------------------------------------------------------------------

    /// URL for downloading a Lima release archive.
    fn lima_archive_url(version: &str, os: &str, arch: &str) -> String {
        format!(
            "https://github.com/lima-vm/lima/releases/download/v{version}/lima-{version}-{os}-{arch}.tar.gz"
        )
    }

    fn lima_archive_sha256(version: &str, os: &str, arch: &str) -> Option<&'static str> {
        match (version, os, arch) {
            ("2.1.0", "Darwin", "arm64") => Some(LIMA_DARWIN_ARM64_ARCHIVE_SHA256),
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // LimaConfig — single source of truth for limactl path, LIMA_HOME, and
    // instance name. All Lima commands go through `lima_command()` to ensure
    // LIMA_HOME is always set.
    // -----------------------------------------------------------------------

    struct LimaConfig {
        limactl: PathBuf,
        lima_home: PathBuf,
        instance: &'static str,
    }

    impl LimaConfig {
        /// Create a `Command` for `limactl` with `LIMA_HOME` already set.
        fn lima_command(&self) -> Command {
            let mut cmd = Command::new(&self.limactl);
            cmd.env("LIMA_HOME", &self.lima_home);
            cmd
        }
    }

    /// Download the Lima release archive to `dest`.
    fn download_lima_archive(
        dest: &Path,
        version: &str,
        os: &str,
        arch: &str,
        expected_sha256: &str,
    ) -> bool {
        let url = lima_archive_url(version, os, arch);
        let status = Command::new("curl")
            .args(["-fsSL", &url, "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => {
                if !build_helpers::verify_sha256(dest, expected_sha256, "Lima archive") {
                    std::fs::remove_file(dest).ok();
                    return false;
                }
                true
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to download Lima archive from {} (exit: {})",
                    url, s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run curl to download Lima archive: {}",
                    e
                );
                false
            }
        }
    }

    /// Extract a Lima tarball into `dest_dir`.
    ///
    /// Lima archives contain paths like `bin/limactl`, `share/lima/templates/`, etc.
    fn extract_lima_archive(archive: &Path, dest_dir: &Path) -> bool {
        if dest_dir.exists() {
            std::fs::remove_dir_all(dest_dir).ok();
        }
        std::fs::create_dir_all(dest_dir).expect("Failed to create lima extraction directory");

        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .status();

        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                println!("cargo:warning=Failed to extract Lima archive (exit: {})", s);
                false
            }
            Err(e) => {
                println!("cargo:warning=Failed to run tar for Lima extraction: {}", e);
                false
            }
        }
    }

    /// Download and cache the Lima installation. Returns the path to the cache
    /// directory containing `bin/limactl` on success.
    fn ensure_lima_cached(version: &str, os: &str, arch: &str) -> Option<PathBuf> {
        let cache_dir = build_helpers::cache_dir(&format!("lima-{version}-{os}-{arch}"));
        let cached_limactl = cache_dir.join("bin/limactl");

        if cached_limactl.exists() {
            println!(
                "cargo:warning=Using cached Lima {} installation from {:?}",
                version, cache_dir
            );
            return Some(cache_dir);
        }

        println!(
            "cargo:warning=Downloading Lima {} for {}-{}...",
            version, os, arch
        );

        let Some(expected_sha256) = lima_archive_sha256(version, os, arch) else {
            println!(
                "cargo:warning=Missing pinned SHA-256 for Lima {} {}-{} archive; refusing download",
                version, os, arch
            );
            return None;
        };

        let downloads_dir = build_helpers::cache_dir("downloads");
        let archive_path = downloads_dir.join(format!("lima-{}-{}-{}.tar.gz", version, os, arch));
        if !download_lima_archive(&archive_path, version, os, arch, expected_sha256) {
            return None;
        }

        if !extract_lima_archive(&archive_path, &cache_dir) {
            return None;
        }

        // Clean up the archive
        std::fs::remove_file(&archive_path).ok();

        if !cached_limactl.exists() {
            println!(
                "cargo:warning=Lima archive extracted but bin/limactl not found in {:?}",
                cache_dir
            );
            return None;
        }

        Some(cache_dir)
    }

    // -----------------------------------------------------------------------
    // Lima instance management (macOS)
    // -----------------------------------------------------------------------

    /// Ensure the peppy Lima instance exists and is running.
    ///
    /// * If the instance does not exist, create and start it with `template`.
    /// * If it exists but is stopped, start it.
    /// * If it is already running, this is a no-op.
    ///
    /// When `arch` is `Some`, an `--arch=<value>` flag is passed to
    /// `limactl start` so the VM runs under QEMU emulation for a
    /// non-native architecture.
    fn ensure_lima_instance(lima: &LimaConfig, template: &str, arch: Option<&str>) -> bool {
        // Query instance status using Go template output — avoids brittle JSON parsing.
        let list_output = lima
            .lima_command()
            .args(["list", "--format", "{{.Status}}", lima.instance])
            .output();

        let instance_status = match &list_output {
            Ok(o) if o.status.success() => {
                let status = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if status.is_empty() {
                    None
                } else {
                    Some(status)
                }
            }
            _ => None,
        };

        match instance_status.as_deref() {
            Some("Running") => {
                // Already running — nothing to do.
                true
            }
            Some(_status) => {
                // Instance exists but is not running — start it.
                println!("cargo:warning=Starting Lima {} instance...", lima.instance);
                let mut cmd = lima.lima_command();
                cmd.args(["start", lima.instance]);
                let label = format!("lima-start-{}", lima.instance);
                build_helpers::run_command_streaming(&mut cmd, &label).success
            }
            None => {
                // Instance does not exist — create and start it.
                println!(
                    "cargo:warning=Creating Lima {} instance with {} (this may take a few minutes on first run)...",
                    lima.instance, template
                );
                let name_flag = format!("--name={}", lima.instance);
                let mut cmd = lima.lima_command();
                cmd.args([
                    "start",
                    &name_flag,
                    "--tty=false",
                    "--mount-writable",
                    "--containerd=none",
                    "--memory=12",
                ]);
                if let Some(a) = arch {
                    cmd.arg(format!("--arch={}", a));
                }
                cmd.arg(template);
                let label = format!("lima-create-{}", lima.instance);
                build_helpers::run_command_streaming(&mut cmd, &label).success
            }
        }
    }

    // -----------------------------------------------------------------------
    // Build apptainer from source
    // -----------------------------------------------------------------------

    /// Build apptainer from source on the local host (native builds only).
    ///
    /// Downloads the source tarball from GitHub, builds with `mconfig` + `make`,
    /// and installs to `install_dir`. Requires Go, make, gcc, libseccomp-dev,
    /// and pkg-config to be available on the host.
    fn build_apptainer_from_source(version: &str, install_dir: &Path) -> bool {
        println!(
            "cargo:warning=Building apptainer {} from source (requires Go, make, gcc, libseccomp-dev)...",
            version
        );

        let source_cache = build_helpers::cache_dir("apptainer-source");

        // Serialize concurrent build invocations to prevent one build from
        // deleting the source tree while another is compiling inside it.
        let lock_path = source_cache.join(".build.lock");
        let _build_lock = build_helpers::acquire_file_lock(&lock_path);

        let tarball_url = format!(
            "https://github.com/apptainer/apptainer/releases/download/v{version}/apptainer-{version}.tar.gz"
        );
        let tarball_path = source_cache.join(format!("apptainer-{version}.tar.gz"));
        let source_dir = source_cache.join(format!("apptainer-{version}"));

        // Clean previous source directory
        if source_dir.exists() {
            std::fs::remove_dir_all(&source_dir).ok();
        }

        // Download source tarball
        if !build_helpers::run_command(
            Command::new("curl")
                .args(["-fsSL", &tarball_url, "-o"])
                .arg(&tarball_path),
            &format!("download apptainer {version} source tarball"),
        ) {
            return false;
        }

        // Extract source tarball
        if !build_helpers::run_command(
            Command::new("tar")
                .args(["-xzf"])
                .arg(&tarball_path)
                .arg("-C")
                .arg(&source_cache),
            "extract apptainer source tarball",
        ) {
            return false;
        }

        // Clean up tarball
        std::fs::remove_file(&tarball_path).ok();

        // The GitHub auto-generated tarball does not include a VERSION file,
        // but apptainer's mconfig requires either .git or VERSION to determine
        // the version.  Create it from the version we already know.
        std::fs::write(source_dir.join("VERSION"), format!("{}\n", version))
            .expect("Failed to write apptainer VERSION file");

        // Refresh vendored Go dependencies — the release tarball's vendor/
        // directory can be stale (vendor/modules.txt out of sync with go.mod).
        if !build_helpers::run_command(
            Command::new("go")
                .current_dir(&source_dir)
                .args(["mod", "vendor"]),
            "refresh apptainer vendor directory",
        ) {
            std::fs::remove_dir_all(&source_dir).ok();
            return false;
        }

        // Start fresh install directory
        force_remove_dir(install_dir);
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        // Configure: ./mconfig --without-suid --prefix=<install_dir>
        // Build without setuid support — apptainer uses unprivileged user
        // namespaces instead.  This avoids the "Relocation not allowed with
        // starter-suid" error that occurs when the compiled-in --prefix
        // doesn't match the final installation path.
        if !build_helpers::run_command(
            Command::new("./mconfig")
                .current_dir(&source_dir)
                .arg("--without-suid")
                .arg(format!("--prefix={}", install_dir.display())),
            "configure apptainer build",
        ) {
            std::fs::remove_dir_all(&source_dir).ok();
            return false;
        }

        // Build: make -C builddir
        if !build_helpers::run_command_streaming(
            Command::new("make")
                .current_dir(&source_dir)
                .args(["-C", "builddir", "-j"]),
            "apptainer-compile",
        )
        .success
        {
            std::fs::remove_dir_all(&source_dir).ok();
            return false;
        }

        // Install: make -C builddir install
        if !build_helpers::run_command(
            Command::new("make")
                .current_dir(&source_dir)
                .args(["-C", "builddir", "install"]),
            "install apptainer",
        ) {
            std::fs::remove_dir_all(&source_dir).ok();
            return false;
        }

        // Clean up source directory
        std::fs::remove_dir_all(&source_dir).ok();

        true
    }

    /// Build apptainer from source inside a Lima VM.
    ///
    /// Starts a Lima VM of the target architecture (using `--arch` for
    /// non-native targets so QEMU emulates the correct ISA), builds
    /// apptainer natively inside it, then copies the result back to the host.
    /// This avoids cross-compilation entirely — the binary is guaranteed to
    /// match the target because it is built on the target architecture.
    fn build_apptainer_from_source_via_lima(
        lima: &LimaConfig,
        version: &str,
        install_dir: &Path,
        target_arch: &str,
    ) -> bool {
        // Map Rust arch names to Lima --arch values.
        let lima_arch = match target_arch {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            other => {
                println!(
                    "cargo:warning=Unsupported Lima VM architecture for apptainer build: {}",
                    other
                );
                return false;
            }
        };

        // Only pass --arch when the target differs from the host.
        let arch_flag = if target_arch != std::env::consts::ARCH {
            Some(lima_arch)
        } else {
            None
        };

        // Cross-arch VMs need a guest agent binary for the target
        // architecture.  The main Lima package only ships the native agent;
        // additional agents come from brew's `lima-additional-guestagents`.
        // Copy any missing agents into the Lima share directory.
        if arch_flag.is_some() {
            let lima_share = lima
                .limactl
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("share/lima");
            let agent_name = format!("lima-guestagent.Linux-{}.gz", lima_arch);
            let dest = lima_share.join(&agent_name);
            if !dest.exists() {
                let brew_src = PathBuf::from("/opt/homebrew/share/lima").join(&agent_name);
                if brew_src.exists() {
                    println!(
                        "cargo:warning=Copying {} guest agent from Homebrew",
                        lima_arch
                    );
                    std::fs::copy(&brew_src, &dest).ok();
                } else {
                    println!(
                        "cargo:warning=Guest agent {} not found; install lima-additional-guestagents via Homebrew",
                        agent_name
                    );
                }
            }
        }

        if !ensure_lima_instance(lima, LIMA_TEMPLATE, arch_flag) {
            println!(
                "cargo:warning=Could not ensure a running Lima instance for apptainer source build"
            );
            return false;
        }

        println!(
            "cargo:warning=Building apptainer {} for {} from source inside Lima VM (this may take several minutes)...",
            version, target_arch
        );

        let guest_install_dir = GUEST_APPTAINER_DIR;

        let build_script = format!(
            r#"set -eu
echo "=== Waiting for apt lock ==="
while sudo fuser /var/lib/apt/lists/lock /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend >/dev/null 2>&1; do sleep 2; done
echo "=== Installing build dependencies ==="
sudo apt-get update -qq
sudo apt-get install -y -qq golang-go libseccomp-dev make gcc pkg-config squashfs-tools cryptsetup
cd /tmp
sudo rm -rf apptainer-{version} apptainer-{version}.tar.gz {guest_install_dir}
echo "=== Downloading apptainer {version} source ==="
curl -fsSL https://github.com/apptainer/apptainer/releases/download/v{version}/apptainer-{version}.tar.gz -o apptainer-{version}.tar.gz
tar -xzf apptainer-{version}.tar.gz
cd apptainer-{version}
echo "{version}" > VERSION
echo "=== Refreshing vendored Go dependencies ==="
go mod vendor
echo "=== Configuring apptainer ==="
./mconfig --without-suid --prefix={guest_install_dir}
echo "=== Compiling apptainer (this is the slow part under QEMU) ==="
make -C builddir -j"$(nproc)"
echo "=== Installing apptainer ==="
make -C builddir install
rm -rf /tmp/apptainer-{version} /tmp/apptainer-{version}.tar.gz
echo "=== Apptainer build complete ==="
"#,
            version = version,
            guest_install_dir = guest_install_dir,
        );

        let label = format!("apptainer-build-{}", target_arch);
        let mut cmd = lima.lima_command();
        cmd.args(["shell", lima.instance, "--", "bash", "-c", &build_script]);
        let run = build_helpers::run_command_streaming(&mut cmd, &label);
        if !run.success {
            return false;
        }

        copy_lima_result_to_host(lima, guest_install_dir, install_dir)
    }

    /// Copy a directory from the Lima guest to the host via tar pipe.
    fn copy_lima_result_to_host(lima: &LimaConfig, guest_dir: &str, host_dir: &Path) -> bool {
        force_remove_dir(host_dir);
        std::fs::create_dir_all(host_dir).expect("Failed to create host install directory");

        let tar_pipe = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "LIMA_HOME='{}' '{}' shell {} -- tar -cf - -C {} . | tar -xf - -C '{}'",
                lima.lima_home.display(),
                lima.limactl.display(),
                lima.instance,
                guest_dir,
                host_dir.display(),
            ))
            .output();
        match &tar_pipe {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Failed to copy installation from Lima VM (exit: {}): {}",
                    o.status, stderr
                );
                return false;
            }
            Err(e) => {
                println!("cargo:warning=Failed to run tar pipe from Lima VM: {}", e);
                return false;
            }
        }

        // Clean up guest temp files
        let _ = lima
            .lima_command()
            .args(["shell", lima.instance, "--", "rm", "-rf", guest_dir])
            .status();

        true
    }

    // -----------------------------------------------------------------------
    // Utilities
    // -----------------------------------------------------------------------

    fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
        if !dst.exists() {
            std::fs::create_dir_all(dst)?;
        }
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let file_type = entry.file_type()?;

            if file_type.is_symlink() {
                let target = std::fs::read_link(&src_path)?;
                std::os::unix::fs::symlink(&target, &dst_path)?;
            } else if file_type.is_dir() {
                copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Main entry point
    // -----------------------------------------------------------------------

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-env-changed=PEPPY_APPTAINER_DIR");
        println!("cargo:rerun-if-env-changed=PEPPY_LIMA_DIR");
        println!("cargo:rerun-if-env-changed=PEPPY_CROSS_ARCH");

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

        println!("cargo:rustc-env=LIMA_INSTANCE={}", LIMA_INSTANCE);
        println!("cargo:rustc-env=LIMA_TEMPLATE={}", LIMA_TEMPLATE);
        println!("cargo:rustc-env=APPTAINER_VERSION={}", APPTAINER_VERSION);
        println!("cargo:rustc-env=LIMA_VERSION={}", LIMA_VERSION);
        println!(
            "cargo:rustc-env=GUEST_APPTAINER_DIR={}",
            GUEST_APPTAINER_DIR
        );

        // On macOS, apptainer is Linux-only and runs inside a Lima VM.
        // We download and bundle Lima ourselves — no `brew install lima` required.
        let use_lima = if target_os == "macos" {
            true
        } else if target_os != "linux" {
            println!(
                "cargo:warning=Skipping apptainer build: apptainer is Linux-only (target_os={})",
                target_os
            );
            return;
        } else {
            false
        };

        // Use the target architecture from the Rust compilation target.
        // CARGO_CFG_TARGET_ARCH reflects the *target* (e.g. "x86_64" when
        // cross-compiling for x86_64-unknown-linux-gnu), not the host.
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "aarch64".to_string());

        let out_dir = env::var("OUT_DIR").unwrap();

        // ------------------------------------------------------------------
        // Step 1 (macOS only): Download and cache Lima
        // ------------------------------------------------------------------
        let lima_config = if use_lima {
            let lima_cache_dir = match ensure_lima_cached(LIMA_VERSION, "Darwin", "arm64") {
                Some(dir) => dir,
                None => {
                    panic!(
                        "Could not download Lima {}. Lima is required for macOS builds.",
                        LIMA_VERSION
                    );
                }
            };

            let lima = LimaConfig {
                limactl: lima_cache_dir.join("bin/limactl"),
                lima_home: get_lima_build_home(),
                instance: LIMA_INSTANCE,
            };

            // Copy Lima installation to OUT_DIR for the crate to reference at compile time
            let out_lima_dir = PathBuf::from(&out_dir).join("lima-install");
            if out_lima_dir.exists() {
                std::fs::remove_dir_all(&out_lima_dir).ok();
            }
            if let Err(e) = copy_dir_recursive(&lima_cache_dir, &out_lima_dir) {
                panic!("Failed to copy Lima installation to OUT_DIR: {}", e);
            }
            println!(
                "cargo:rustc-env=LIMA_INSTALL_DIR={}",
                out_lima_dir.display()
            );
            println!(
                "cargo:rustc-env=LIMA_BUILD_HOME={}",
                lima.lima_home.display()
            );

            Some(lima)
        } else {
            None
        };

        // ------------------------------------------------------------------
        // Step 2: Build apptainer from source
        // ------------------------------------------------------------------

        // On macOS, build apptainer inside Lima VMs.  By default only the
        // native architecture is built.  Set PEPPY_CROSS_ARCH=1 (used by the
        // release script) to also build for non-native architectures so the
        // cache is ready for cross-compiled release targets.
        if use_lima && let Some(ref lima) = lima_config {
            let cross_arch = env::var("PEPPY_CROSS_ARCH").unwrap_or_default() == "1";
            let native_arch: &str = std::env::consts::ARCH;
            let targets: Vec<&str> = if cross_arch {
                vec!["aarch64", "x86_64"]
            } else {
                vec![native_arch]
            };
            for target in &targets {
                let target_cache = build_helpers::cache_dir(&format!(
                    "apptainer-{}-{}-nosuid",
                    APPTAINER_VERSION, target
                ));
                let sentinel = apptainer_cache_sentinel_path(&target_cache, APPTAINER_VERSION);
                if sentinel.exists() && target_cache.join("bin/apptainer").exists() {
                    println!(
                        "cargo:warning=Apptainer {} for {} already cached",
                        APPTAINER_VERSION, target
                    );
                    continue;
                }
                println!(
                    "cargo:warning=Pre-building apptainer {} for {} via Lima VM...",
                    APPTAINER_VERSION, target
                );
                // Each architecture gets its own Lima instance so the VM
                // runs natively on the target ISA (or under QEMU emulation
                // for cross-arch).
                let instance_name: &'static str = match *target {
                    "aarch64" => "peppy-a64",
                    "x86_64" => "peppy-x64",
                    _ => LIMA_INSTANCE,
                };
                let target_lima = LimaConfig {
                    limactl: lima.limactl.clone(),
                    lima_home: lima.lima_home.clone(),
                    instance: instance_name,
                };
                let ok = build_apptainer_from_source_via_lima(
                    &target_lima,
                    APPTAINER_VERSION,
                    &target_cache,
                    target,
                );
                assert!(
                    ok,
                    "Failed to build apptainer {} for {} in Lima VM",
                    APPTAINER_VERSION, target
                );
                assert!(
                    target_cache.join("bin/apptainer").exists(),
                    "Apptainer build for {} completed but bin/apptainer missing",
                    target
                );
                std::fs::write(&sentinel, format!("version={}\n", APPTAINER_VERSION))
                    .unwrap_or_else(|e| {
                        panic!("Failed to write cache sentinel {:?}: {}", sentinel, e)
                    });
            }
        }

        let cache_dir =
            build_helpers::cache_dir(&format!("apptainer-{}-{}-nosuid", APPTAINER_VERSION, &arch));

        // On Linux inside a Lima VM, the macOS-side cache is accessible at the
        // same absolute path because Lima mounts the host home directory.
        // Check there as a fallback when the Linux-side cache is empty.
        let macos_cache_hit = if !use_lima {
            let sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
            if !sentinel.exists() || !cache_dir.join("bin/apptainer").exists() {
                // The Linux HOME-based cache_dir didn't hit.  Try the macOS
                // home path (e.g. /Users/<user>/.peppy/tmp/...) which Lima
                // mounts into the guest.
                let macos_home = PathBuf::from("/Users");
                if macos_home.is_dir() {
                    // Find any matching cache under /Users/*/.peppy/tmp/
                    let pattern = format!("apptainer-{}-{}-nosuid", APPTAINER_VERSION, &arch);
                    let mut found = false;
                    if let Ok(entries) = std::fs::read_dir(&macos_home) {
                        for entry in entries.flatten() {
                            let candidate = entry.path().join(".peppy/tmp").join(&pattern);
                            let candidate_sentinel =
                                apptainer_cache_sentinel_path(&candidate, APPTAINER_VERSION);
                            if candidate_sentinel.exists()
                                && candidate.join("bin/apptainer").exists()
                            {
                                println!(
                                    "cargo:warning=Using macOS-side cached apptainer from {:?}",
                                    candidate
                                );
                                // Copy to our local cache so OUT_DIR copy works.
                                if cache_dir.exists() {
                                    std::fs::remove_dir_all(&cache_dir).ok();
                                }
                                copy_dir_recursive(&candidate, &cache_dir)
                                    .expect("Failed to copy macOS apptainer cache");
                                std::fs::write(
                                    apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION),
                                    format!("version={}\n", APPTAINER_VERSION),
                                )
                                .ok();
                                found = true;
                                break;
                            }
                        }
                    }
                    found
                } else {
                    false
                }
            } else {
                true // Linux-side cache hit
            }
        } else {
            false // macOS path already handled above
        };

        // Check if we have a fully completed cached installation.
        let cached_bin = cache_dir.join("bin/apptainer");
        let cache_sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
        if cache_sentinel.exists() && cached_bin.exists() {
            println!(
                "cargo:warning=Using cached apptainer installation from {:?}",
                cache_dir
            );
        } else if macos_cache_hit {
            // Already handled above via copy.
        } else {
            println!(
                "cargo:warning=Building apptainer {} from source...",
                APPTAINER_VERSION
            );

            let success = build_apptainer_from_source(APPTAINER_VERSION, &cache_dir);

            assert!(
                success,
                "Failed to build apptainer {} from source for {}. \
                 Ensure Go, make, gcc, libseccomp-dev, and pkg-config are installed.",
                APPTAINER_VERSION, arch
            );

            assert!(
                cached_bin.exists(),
                "Apptainer source build completed but bin/apptainer not found in {:?}",
                cache_dir
            );

            std::fs::write(&cache_sentinel, format!("version={}\n", APPTAINER_VERSION))
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to write apptainer cache sentinel {:?}: {}",
                        cache_sentinel, e
                    )
                });
        }

        // Copy apptainer installation to OUT_DIR so the release packaging
        // script can find it via containers-*/out/apptainer-install glob.
        let out_install_dir = PathBuf::from(&out_dir).join("apptainer-install");
        if out_install_dir.exists() {
            std::fs::remove_dir_all(&out_install_dir).ok();
        }
        copy_dir_recursive(&cache_dir, &out_install_dir)
            .unwrap_or_else(|e| panic!("Failed to copy apptainer installation to OUT_DIR: {}", e));

        println!(
            "cargo:rustc-env=APPTAINER_INSTALL_DIR={}",
            cache_dir.display()
        );
    }
}

fn main() {
    apptainer_build::run();
}
