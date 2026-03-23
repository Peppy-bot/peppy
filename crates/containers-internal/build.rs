mod apptainer_build {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const APPTAINER_VERSION: &str = "1.4.5";
    const LIMA_VERSION: &str = "2.0.3";
    const LIMA_DARWIN_ARM64_ARCHIVE_SHA256: &str =
        "22aee997df59e4fd448041b2d1214e48bd8eaf705d2d48a4307d65c1b179dc97";
    const LIMA_INSTANCE: &str = "peppy";
    const LIMA_TEMPLATE: &str = "template:ubuntu-24.04";
    /// Guest-side installation path for apptainer inside the Lima VM.
    /// Must match the `--prefix` used at build time so `starter-suid` doesn't
    /// reject the binary as relocated.
    const GUEST_APPTAINER_DIR: &str = "/tmp/peppy/apptainer";

    // -----------------------------------------------------------------------
    // Cache helpers
    // -----------------------------------------------------------------------

    fn apptainer_cache_sentinel_path(cache_dir: &Path, version: &str) -> PathBuf {
        cache_dir.join(format!(".peppy-version-{}", version))
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
            ("2.0.3", "Darwin", "arm64") => Some(LIMA_DARWIN_ARM64_ARCHIVE_SHA256),
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
    fn ensure_lima_instance(lima: &LimaConfig, template: &str) -> bool {
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
                let start = lima.lima_command().args(["start", lima.instance]).output();
                match start {
                    Ok(o) if o.status.success() => true,
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "cargo:warning=Failed to start Lima {} instance (exit: {}): {}",
                            lima.instance, o.status, stderr
                        );
                        false
                    }
                    Err(e) => {
                        println!("cargo:warning=Failed to run limactl start: {}", e);
                        false
                    }
                }
            }
            None => {
                // Instance does not exist — create and start it.
                println!(
                    "cargo:warning=Creating Lima {} instance with {} (this may take a few minutes on first run)...",
                    lima.instance, template
                );
                let name_flag = format!("--name={}", lima.instance);
                let create = lima
                    .lima_command()
                    .args([
                        "start",
                        &name_flag,
                        "--tty=false",
                        "--mount-writable",
                        "--memory=12",
                        template,
                    ])
                    .output();
                match create {
                    Ok(o) if o.status.success() => true,
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "cargo:warning=Failed to create Lima {} instance (exit: {}): {}",
                            lima.instance, o.status, stderr
                        );
                        false
                    }
                    Err(e) => {
                        println!("cargo:warning=Failed to run limactl start: {}", e);
                        false
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Build apptainer from source
    // -----------------------------------------------------------------------

    /// Build apptainer from source on the local host (native Linux builds).
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

        // Start fresh install directory
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        // Configure: ./mconfig --prefix=<install_dir>
        if !build_helpers::run_command(
            Command::new("./mconfig")
                .current_dir(&source_dir)
                .arg(format!("--prefix={}", install_dir.display())),
            "configure apptainer build",
        ) {
            std::fs::remove_dir_all(&source_dir).ok();
            return false;
        }

        // Build: make -C builddir
        if !build_helpers::run_command(
            Command::new("make")
                .current_dir(&source_dir)
                .args(["-C", "builddir", "-j"]),
            "compile apptainer",
        ) {
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

        // Create starter-suid as a copy of starter. In Apptainer, both are the
        // same binary — the difference is that starter-suid has the setuid bit
        // (set at install time by scripts/install.sh since it requires root).
        // `make install` skips creating starter-suid for non-root builds, so we
        // create the copy here.
        let starter = install_dir.join("libexec/apptainer/bin/starter");
        let starter_suid = install_dir.join("libexec/apptainer/bin/starter-suid");
        if starter.exists() && !starter_suid.exists() {
            std::fs::copy(&starter, &starter_suid)
                .expect("Failed to create starter-suid copy of starter");
        }

        true
    }

    /// Build apptainer from source inside a Lima VM (macOS builds for Linux targets).
    ///
    /// Downloads and builds apptainer inside the VM, then copies the result
    /// back to the host.
    fn build_apptainer_from_source_via_lima(
        lima: &LimaConfig,
        version: &str,
        install_dir: &Path,
    ) -> bool {
        if !ensure_lima_instance(lima, LIMA_TEMPLATE) {
            println!(
                "cargo:warning=Could not ensure a running Lima instance for apptainer source build"
            );
            return false;
        }

        println!(
            "cargo:warning=Building apptainer {} from source inside Lima VM (this may take several minutes)...",
            version
        );

        let guest_install_dir = GUEST_APPTAINER_DIR;
        let build_script = format!(
            r#"set -eu
sudo apt-get update -qq
sudo apt-get install -y -qq golang-go libseccomp-dev make gcc pkg-config squashfs-tools cryptsetup > /dev/null 2>&1
cd /tmp
rm -rf apptainer-{version} apptainer-{version}.tar.gz {guest_install_dir}
curl -fsSL https://github.com/apptainer/apptainer/releases/download/v{version}/apptainer-{version}.tar.gz -o apptainer-{version}.tar.gz
tar -xzf apptainer-{version}.tar.gz
cd apptainer-{version}
./mconfig --prefix={guest_install_dir}
make -C builddir -j
make -C builddir install
cp {guest_install_dir}/libexec/apptainer/bin/starter {guest_install_dir}/libexec/apptainer/bin/starter-suid
rm -rf /tmp/apptainer-{version} /tmp/apptainer-{version}.tar.gz"#,
            version = version,
            guest_install_dir = guest_install_dir,
        );

        let run = lima
            .lima_command()
            .args(["shell", lima.instance, "--", "bash", "-c", &build_script])
            .output();
        match &run {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                println!(
                    "cargo:warning=Apptainer source build failed inside Lima VM (exit: {}): {} {}",
                    o.status, stderr, stdout
                );
                return false;
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run apptainer source build via limactl shell: {}",
                    e
                );
                return false;
            }
        }

        copy_lima_result_to_host(lima, guest_install_dir, install_dir)
    }

    /// Copy a directory from the Lima guest to the host via tar pipe.
    fn copy_lima_result_to_host(lima: &LimaConfig, guest_dir: &str, host_dir: &Path) -> bool {
        if host_dir.exists() {
            std::fs::remove_dir_all(host_dir).ok();
        }
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

        // On macOS via Lima, the guest architecture may differ from the host.
        // Default to aarch64 since macOS builds are Apple Silicon only.
        let arch = if use_lima {
            "aarch64".to_string()
        } else {
            env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string())
        };

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
        let cache_dir =
            build_helpers::cache_dir(&format!("apptainer-{}-{}-src", APPTAINER_VERSION, &arch));

        // Check if we have a fully completed cached installation.
        let cached_bin = cache_dir.join("bin/apptainer");
        let cache_sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
        if cache_sentinel.exists() && cached_bin.exists() {
            println!(
                "cargo:warning=Using cached apptainer installation from {:?}",
                cache_dir
            );
        } else {
            println!(
                "cargo:warning=Building apptainer {} from source...",
                APPTAINER_VERSION
            );

            let success = if let Some(ref lima) = lima_config {
                build_apptainer_from_source_via_lima(lima, APPTAINER_VERSION, &cache_dir)
            } else {
                build_apptainer_from_source(APPTAINER_VERSION, &cache_dir)
            };

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
