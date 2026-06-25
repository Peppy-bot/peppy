mod apptainer_build {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    const APPTAINER_VERSION: &str = "1.5.2";
    /// SHA-256 of `apptainer-{APPTAINER_VERSION}.tar.gz` from the GitHub release.
    /// Bump alongside `APPTAINER_VERSION`; both `verify_apptainer_checksum`
    /// call sites (host build, Lima guest build) consume this constant.
    const APPTAINER_SHA256: &str =
        "0dc689f4b1036941837f38376313082d953eec920520e295525d89e0f0e04f98";

    /// Pinned gocryptfs version. Apptainer auto-discovers gocryptfs in
    /// `${prefix}/libexec/apptainer/bin/` (ahead of `$PATH`) and uses it for
    /// encrypted overlay/image support. Shipping it alongside apptainer means
    /// users don't need to install it via the system package manager.
    const GOCRYPTFS_VERSION: &str = "2.6.1";
    /// SHA-256 of `gocryptfs_v{GOCRYPTFS_VERSION}_linux-static_amd64.tar.gz`.
    const GOCRYPTFS_AMD64_SHA256: &str =
        "49b8c0eb0f6373b6ac99c394a52909d8478e74c08d0961527c1162967cc28c44";
    /// SHA-256 of `gocryptfs_v{GOCRYPTFS_VERSION}_linux-static_arm64.tar.gz`.
    const GOCRYPTFS_ARM64_SHA256: &str =
        "64576d550ab8af3f1dc729e93779540c5ecc00967d0185aae51a29a3755d86d0";

    const LIMA_VERSION: &str = "2.1.3";
    const LIMA_DARWIN_ARM64_ARCHIVE_SHA256: &str =
        "52bcf0780fcb28128ac9f6924d4410a6bc7c92fa80c9a858d89ae34ec3ce4f35";
    /// SHA-256 of `lima-additional-guestagents-{LIMA_VERSION}-Darwin-arm64.tar.gz`.
    /// This archive carries the cross-architecture (Linux-x86_64, ...) guest
    /// agents that the main Lima package omits. The guest agent MUST come from
    /// the same pinned Lima version as the host `limactl`: a version-skewed
    /// agent cannot speak to the host and leaves cross-arch VMs stuck in a
    /// DEGRADED state. Bump alongside `LIMA_VERSION`.
    const LIMA_ADDITIONAL_GUESTAGENTS_DARWIN_ARM64_SHA256: &str =
        "ee85b79aa7ebebf71039d6fb145695c5697ff870f6c88bf4150a6bd72813b78c";
    const LIMA_INSTANCE: &str = "peppy";

    /// Prebuilt amd64 Ubuntu base rootfs. On an Apple Silicon host the x86_64
    /// apptainer build runs the amd64 toolchain under Rosetta 2 inside a native
    /// aarch64 VZ VM (instead of slow QEMU full-system emulation). We `chroot`
    /// into this rootfs so every build binary is amd64 and emits genuine x86_64
    /// output. Pinned + SHA-verified like every other download. Bump together.
    const UBUNTU_BASE_VERSION: &str = "24.04.4";
    const UBUNTU_BASE_AMD64_SHA256: &str =
        "c1e67ef7b17a6300e136118bd1dc04725009cb376c1aad10abcf8cd453628d58";

    /// Apt packages needed to build apptainer from source. Shared by the native
    /// build (full guest) and the Rosetta build (minimal amd64 chroot, which is
    /// why `curl`/`ca-certificates` are listed explicitly).
    const APPTAINER_BUILD_DEPS: &str = "golang-go libseccomp-dev make gcc pkg-config squashfs-tools cryptsetup curl ca-certificates";
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

    fn write_cache_sentinel(cache_dir: &Path, version: &str) {
        let sentinel = apptainer_cache_sentinel_path(cache_dir, version);
        std::fs::write(&sentinel, format!("version={}\n", version))
            .unwrap_or_else(|e| panic!("Failed to write cache sentinel {:?}: {}", sentinel, e));
    }

    fn apptainer_cache_dir(version: &str, arch: &str) -> PathBuf {
        build_helpers::cache_dir(&format!("apptainer-{}-{}-nosuid", version, arch))
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

    /// URL for the `lima-additional-guestagents` archive (cross-arch guest
    /// agents). The host is always Darwin/arm64, so we only need that variant;
    /// it bundles the Linux-x86_64 (and other) agents pushed into cross-arch VMs.
    fn lima_additional_guestagents_url(version: &str) -> String {
        format!(
            "https://github.com/lima-vm/lima/releases/download/v{version}/lima-additional-guestagents-{version}-Darwin-arm64.tar.gz"
        )
    }

    /// URL for the prebuilt amd64 Ubuntu base rootfs (Rosetta build path).
    fn ubuntu_base_amd64_url(version: &str) -> String {
        // The release directory is keyed by the `major.minor` series even though
        // the tarball name carries the full point-release version.
        format!(
            "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-{version}-base-amd64.tar.gz"
        )
    }

    fn lima_archive_sha256(version: &str, os: &str, arch: &str) -> Option<&'static str> {
        match (version, os, arch) {
            ("2.1.3", "Darwin", "arm64") => Some(LIMA_DARWIN_ARM64_ARCHIVE_SHA256),
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

    /// Download `url` to `dest` and verify it against `expected_sha256`.
    /// Deletes the file and returns false on download failure or checksum
    /// mismatch (so a corrupt download is never reused). `label` names the
    /// artifact in log messages.
    fn download_and_verify(url: &str, dest: &Path, expected_sha256: &str, label: &str) -> bool {
        let status = Command::new("curl")
            .args(["-fsSL", url, "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => {
                if build_helpers::verify_sha256(dest, expected_sha256, label) {
                    return true;
                }
                std::fs::remove_file(dest).ok();
                false
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to download {} from {} (exit: {})",
                    label, url, s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run curl to download {}: {}",
                    label, e
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
        let url = lima_archive_url(version, os, arch);
        if !download_and_verify(&url, &archive_path, expected_sha256, "Lima archive") {
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

    /// Ensure the cross-architecture guest agent
    /// `lima-guestagent.Linux-{lima_arch}.gz` is present in the cached Lima
    /// `share/lima` directory and comes from the pinned [`LIMA_VERSION`].
    ///
    /// The main Lima archive ships only the host-native agent, so cross-arch
    /// VMs (x86_64 under QEMU on Apple Silicon) need their agent supplied
    /// separately. Sourcing it from the pinned `lima-additional-guestagents`
    /// release (rather than whatever version Homebrew happens to have installed)
    /// keeps host and guest agent in lockstep; a mismatch leaves the VM stuck in
    /// a DEGRADED state because the host cannot reach the guest agent.
    ///
    /// The agent is copied unconditionally so a previously cached, version-
    /// skewed agent (e.g. a Homebrew copy left by an older build) is replaced.
    fn ensure_cross_guest_agent(lima_share: &Path, lima_arch: &str) -> bool {
        let agent_name = format!("lima-guestagent.Linux-{}.gz", lima_arch);
        let extract_dir = build_helpers::cache_dir(&format!(
            "lima-additional-guestagents-{}-Darwin-arm64",
            LIMA_VERSION
        ));
        let src_agent = extract_dir.join("share/lima").join(&agent_name);

        if !src_agent.exists() {
            println!(
                "cargo:warning=Fetching pinned Lima {} guest agent for {}...",
                LIMA_VERSION, lima_arch
            );
            let downloads_dir = build_helpers::cache_dir("downloads");
            let archive_path = downloads_dir.join(format!(
                "lima-additional-guestagents-{}-Darwin-arm64.tar.gz",
                LIMA_VERSION
            ));
            let url = lima_additional_guestagents_url(LIMA_VERSION);
            if !download_and_verify(
                &url,
                &archive_path,
                LIMA_ADDITIONAL_GUESTAGENTS_DARWIN_ARM64_SHA256,
                "Lima additional guest agents archive",
            ) {
                return false;
            }
            let extracted = extract_lima_archive(&archive_path, &extract_dir);
            std::fs::remove_file(&archive_path).ok();
            if !extracted {
                return false;
            }
        }

        if !src_agent.exists() {
            println!(
                "cargo:warning=Guest agent {} missing from pinned Lima {} additional agents",
                agent_name, LIMA_VERSION
            );
            return false;
        }

        let dest = lima_share.join(&agent_name);
        if let Err(e) = std::fs::copy(&src_agent, &dest) {
            println!(
                "cargo:warning=Failed to install guest agent {} into {:?}: {}",
                agent_name, lima_share, e
            );
            return false;
        }
        true
    }

    // -----------------------------------------------------------------------
    // Lima instance management (macOS)
    // -----------------------------------------------------------------------

    /// Maximum time to wait for the guest reachability probe before treating
    /// the instance as unusable. A healthy guest answers in about a second; the
    /// generous budget only matters for a wedged guest we are about to recreate.
    const LIMA_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Whether the x86_64 apptainer build can take the fast Rosetta path: a
    /// native aarch64 macOS host with Rosetta 2 installed. Rosetta translates
    /// the amd64 build toolchain inside a native aarch64 VZ VM, far faster than
    /// QEMU full-system emulation, while still emitting genuine x86_64 binaries.
    /// Set `PEPPY_NO_ROSETTA=1` to force the QEMU fallback.
    fn rosetta_available() -> bool {
        env::var("PEPPY_NO_ROSETTA").unwrap_or_default() != "1"
            && std::env::consts::ARCH == "aarch64"
            && std::env::consts::OS == "macos"
            && Path::new("/Library/Apple/usr/libexec/oah").exists()
    }

    /// vCPUs to give a build VM: most of the host's cores, leaving two for the
    /// host. More cores speed up `make -j` and engage multi-threaded TCG on the
    /// QEMU fallback path.
    fn build_vm_cpus() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2))
            .unwrap_or(4)
            .clamp(2, 8)
    }

    /// Query an instance's hypervisor type (`vz`, `qemu`, ...). Returns `None`
    /// when the instance does not exist or the query fails. Used to detect a
    /// stale instance whose VM type no longer matches the desired strategy
    /// (e.g. a leftover QEMU x86_64 VM after switching to the Rosetta path).
    fn lima_instance_vmtype(lima: &LimaConfig) -> Option<String> {
        let output = lima
            .lima_command()
            .args(["list", "--format", "{{.VMType}}", lima.instance])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let vmtype = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!vmtype.is_empty()).then_some(vmtype)
    }

    /// Ensure the peppy Lima instance exists and its guest is reachable.
    ///
    /// * If the instance does not exist, create and start it with `template`.
    /// * If it exists but is stopped, start it.
    /// * If it is running (or just started) but its guest is unreachable, delete
    ///   it and recreate it from scratch.
    ///
    /// The last case matters because a `limactl start` that times out on guest
    /// networking leaves the VM process alive, so the instance reports `Running`
    /// while its guest never finished booting. A plain status check would treat
    /// that corpse as usable, so we verify reachability with an SSH probe and
    /// recreate any instance that fails it.
    ///
    /// When `arch` is `Some`, an `--arch=<value>` flag is passed to
    /// `limactl start` so the VM runs under QEMU emulation for a
    /// non-native architecture.
    fn ensure_lima_instance(
        lima: &LimaConfig,
        template: &str,
        arch: Option<&str>,
        rosetta: bool,
    ) -> bool {
        // Rosetta and native builds run on VZ; only a QEMU cross-arch fallback
        // uses qemu. If an existing instance has the wrong backend (e.g. a
        // leftover QEMU x86_64 VM from before the Rosetta switch), delete it so
        // it is recreated with the right one instead of being silently reused.
        let desired_vmtype = if arch.is_some() && !rosetta {
            "qemu"
        } else {
            "vz"
        };
        if lima_instance_vmtype(lima).is_some_and(|t| t != desired_vmtype) {
            println!(
                "cargo:warning=Lima {} has the wrong VM type for this build; recreating it as {}...",
                lima.instance, desired_vmtype
            );
            delete_lima_instance(lima);
        }

        match lima_instance_status(lima) {
            None => {
                create_lima_instance(lima, template, arch, rosetta);
            }
            Some(status) if status == "Running" => {}
            Some(_) => {
                println!("cargo:warning=Starting Lima {} instance...", lima.instance);
                start_existing_lima_instance(lima);
            }
        }

        // `limactl start` can exit non-zero when the guest agent is unreachable
        // (seen with cross-arch VMs under QEMU emulation) even though the guest
        // is fully usable over SSH. The build only ever drives the guest via
        // `limactl shell` (SSH), so SSH reachability, not the start exit code or
        // guest-agent health, is the authoritative readiness signal. The
        // create/start results above are therefore intentionally not consumed.
        if lima_guest_reachable(lima) {
            return true;
        }

        println!(
            "cargo:warning=Lima {} instance is unusable (guest unreachable over SSH); deleting and recreating it from scratch...",
            lima.instance
        );
        delete_lima_instance(lima);
        create_lima_instance(lima, template, arch, rosetta);
        lima_guest_reachable(lima)
    }

    /// Query an instance's status via Go-template output (avoids brittle JSON
    /// parsing). Returns `None` when the instance does not exist or the query
    /// itself fails.
    fn lima_instance_status(lima: &LimaConfig) -> Option<String> {
        let output = lima
            .lima_command()
            .args(["list", "--format", "{{.Status}}", lima.instance])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!status.is_empty()).then_some(status)
    }

    /// Check whether the guest is actually reachable by running a trivial
    /// command over Lima's SSH connection. A failed or timed-out probe means
    /// the instance is up at the hypervisor level but its guest never came up.
    fn lima_guest_reachable(lima: &LimaConfig) -> bool {
        let mut cmd = lima.lima_command();
        cmd.args(["shell", lima.instance, "--", "true"]);
        build_helpers::run_command_with_timeout(&mut cmd, LIMA_PROBE_TIMEOUT).success
    }

    /// Start an existing, stopped instance.
    fn start_existing_lima_instance(lima: &LimaConfig) -> bool {
        let mut cmd = lima.lima_command();
        cmd.args(["start", lima.instance]);
        let label = format!("lima-start-{}", lima.instance);
        build_helpers::run_command_streaming(&mut cmd, &label).success
    }

    /// Force-delete an instance (stops it first if running). Best-effort: a
    /// failure is logged but not fatal, since the following create is what
    /// determines success.
    fn delete_lima_instance(lima: &LimaConfig) {
        let mut cmd = lima.lima_command();
        cmd.args(["delete", "--force", lima.instance]);
        let label = format!("lima-delete-{}", lima.instance);
        if !build_helpers::run_command_streaming(&mut cmd, &label).success {
            println!(
                "cargo:warning=Failed to delete Lima {} instance; recreate may fail",
                lima.instance
            );
        }
    }

    /// Create and start a fresh instance via `limactl start`. The returned
    /// success flag is advisory: `ensure_lima_instance` confirms real
    /// readiness with an SSH reachability probe, because `limactl start` can
    /// exit non-zero on guest-agent issues that do not affect SSH usability.
    fn create_lima_instance(
        lima: &LimaConfig,
        template: &str,
        arch: Option<&str>,
        rosetta: bool,
    ) -> bool {
        println!(
            "cargo:warning=Creating Lima {} instance with {} (this may take a few minutes on first run)...",
            lima.instance, template
        );
        let name_flag = format!("--name={}", lima.instance);
        let cpus_flag = format!("--cpus={}", build_vm_cpus());
        let mut cmd = lima.lima_command();
        cmd.args([
            "start",
            &name_flag,
            "--tty=false",
            "--mount-writable",
            "--containerd=none",
            "--memory=12",
            &cpus_flag,
        ]);
        if rosetta {
            // Native aarch64 VZ VM with Rosetta: the x86_64 toolchain runs
            // translated, so no `--arch` (which would force QEMU emulation).
            cmd.args(["--vm-type=vz", "--rosetta"]);
        } else if let Some(a) = arch {
            cmd.arg(format!("--arch={}", a));
        }
        cmd.arg(template);
        let label = format!("lima-create-{}", lima.instance);
        build_helpers::run_command_streaming(&mut cmd, &label).success
    }

    // -----------------------------------------------------------------------
    // Apptainer tarball integrity
    // -----------------------------------------------------------------------

    /// Verify a downloaded apptainer source tarball against the pinned
    /// [`APPTAINER_SHA256`]. Returns `true` on match. On mismatch, deletes
    /// the file (so a stale corrupted download isn't reused) and returns false.
    fn verify_apptainer_checksum(tarball: &Path) -> bool {
        if build_helpers::verify_sha256(tarball, APPTAINER_SHA256, "apptainer source tarball") {
            return true;
        }
        std::fs::remove_file(tarball).ok();
        false
    }

    // -----------------------------------------------------------------------
    // gocryptfs — bundled prebuilt static linux binary
    //
    // Apptainer searches `${prefix}/libexec/apptainer/bin/` for tools like
    // gocryptfs before falling back to `$PATH`. Dropping the binary there
    // means encrypted overlay/image support works out of the box without
    // requiring users to install gocryptfs via their distro package manager.
    // -----------------------------------------------------------------------

    /// Map a Rust target arch to gocryptfs's release naming convention.
    fn gocryptfs_arch(target_arch: &str) -> Option<&'static str> {
        match target_arch {
            "x86_64" => Some("amd64"),
            "aarch64" => Some("arm64"),
            _ => None,
        }
    }

    fn gocryptfs_sha256(target_arch: &str) -> Option<&'static str> {
        match target_arch {
            "x86_64" => Some(GOCRYPTFS_AMD64_SHA256),
            "aarch64" => Some(GOCRYPTFS_ARM64_SHA256),
            _ => None,
        }
    }

    fn gocryptfs_archive_url(version: &str, arch: &str) -> String {
        format!(
            "https://github.com/rfjakob/gocryptfs/releases/download/v{version}/gocryptfs_v{version}_linux-static_{arch}.tar.gz"
        )
    }

    fn gocryptfs_sentinel_path(install_dir: &Path) -> PathBuf {
        install_dir
            .join("libexec/apptainer/bin")
            .join(format!(".peppy-gocryptfs-version-{}", GOCRYPTFS_VERSION))
    }

    /// Ensure `<install_dir>/libexec/apptainer/bin/gocryptfs` exists and matches
    /// the pinned [`GOCRYPTFS_VERSION`]. Idempotent: returns immediately when
    /// the sentinel + binary are already in place.
    fn ensure_gocryptfs_installed(install_dir: &Path, target_arch: &str) -> bool {
        let bin_dir = install_dir.join("libexec/apptainer/bin");
        let gocryptfs_bin = bin_dir.join("gocryptfs");
        let gocryptfs_xray_bin = bin_dir.join("gocryptfs-xray");
        let sentinel = gocryptfs_sentinel_path(install_dir);

        if sentinel.exists() && gocryptfs_bin.exists() && gocryptfs_xray_bin.exists() {
            return true;
        }

        let Some(arch) = gocryptfs_arch(target_arch) else {
            println!(
                "cargo:warning=Skipping gocryptfs bundle: unsupported target arch {}",
                target_arch
            );
            return false;
        };
        let Some(expected_sha256) = gocryptfs_sha256(target_arch) else {
            println!(
                "cargo:warning=Skipping gocryptfs bundle: no pinned SHA-256 for {}",
                target_arch
            );
            return false;
        };

        println!(
            "cargo:warning=Installing gocryptfs {} ({}) into {:?}",
            GOCRYPTFS_VERSION, arch, bin_dir
        );

        let downloads_dir = build_helpers::cache_dir("downloads");
        let archive_path = downloads_dir.join(format!(
            "gocryptfs_v{}_linux-static_{}.tar.gz",
            GOCRYPTFS_VERSION, arch
        ));

        // Serialize concurrent download/extract attempts so parallel target
        // builds don't race on the shared archive in the downloads cache.
        let lock_path = downloads_dir.join(format!(".gocryptfs-{}.lock", arch));
        let _lock = build_helpers::acquire_file_lock(&lock_path);

        if !download_gocryptfs_archive(&archive_path, GOCRYPTFS_VERSION, arch, expected_sha256) {
            return false;
        }

        if let Err(e) = std::fs::create_dir_all(&bin_dir) {
            println!(
                "cargo:warning=Failed to create gocryptfs install directory {:?}: {}",
                bin_dir, e
            );
            return false;
        }

        if !extract_gocryptfs_binaries(&archive_path, &bin_dir) {
            return false;
        }

        // Clean up the archive — only useful for the one-shot install.
        std::fs::remove_file(&archive_path).ok();

        if !gocryptfs_bin.exists() {
            println!(
                "cargo:warning=gocryptfs extracted but binary missing at {:?}",
                gocryptfs_bin
            );
            return false;
        }

        // Sentinel marks the cache as up-to-date for the pinned version.
        std::fs::write(
            &sentinel,
            format!("version={}\narch={}\n", GOCRYPTFS_VERSION, arch),
        )
        .unwrap_or_else(|e| panic!("Failed to write gocryptfs sentinel {:?}: {}", sentinel, e));

        true
    }

    fn download_gocryptfs_archive(
        dest: &Path,
        version: &str,
        arch: &str,
        expected_sha256: &str,
    ) -> bool {
        // Reuse an already-downloaded archive when its checksum still matches.
        if dest.exists() && build_helpers::verify_sha256(dest, expected_sha256, "gocryptfs archive")
        {
            return true;
        }

        let url = gocryptfs_archive_url(version, arch);
        let status = Command::new("curl")
            .args(["-fsSL", &url, "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => {
                if !build_helpers::verify_sha256(dest, expected_sha256, "gocryptfs archive") {
                    std::fs::remove_file(dest).ok();
                    return false;
                }
                true
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to download gocryptfs from {} (exit: {})",
                    url, s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to invoke curl for gocryptfs download: {}",
                    e
                );
                false
            }
        }
    }

    /// Extract `gocryptfs` and `gocryptfs-xray` from the release tarball
    /// directly into `dest_dir` (flattening the archive's flat layout).
    fn extract_gocryptfs_binaries(archive: &Path, dest_dir: &Path) -> bool {
        // The gocryptfs release tarball is flat (no leading directory), so a
        // simple `tar -xzf` into the target dir places the binaries directly.
        // Restrict to the two binaries — we don't need the manpages.
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .args(["gocryptfs", "gocryptfs-xray"])
            .status();

        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to extract gocryptfs binaries (exit: {})",
                    s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to invoke tar for gocryptfs extract: {}",
                    e
                );
                false
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

        // Verify integrity before touching the contents.
        if !verify_apptainer_checksum(&tarball_path) {
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

    /// Build apptainer from source inside a Lima VM and copy the result back.
    ///
    /// Strategy by target:
    /// * Native arch: build directly in a VZ VM.
    /// * x86_64 on an Apple Silicon host with Rosetta: build inside an amd64
    ///   `chroot` in a native aarch64 VZ VM, so the toolchain runs under Rosetta
    ///   (fast) yet emits genuine x86_64 binaries.
    /// * x86_64 without Rosetta: fall back to a QEMU full-system VM (slow).
    ///
    /// Every path builds from source for the target ISA, so the binary is
    /// guaranteed to match the target.
    fn build_apptainer_from_source_via_lima(
        lima: &LimaConfig,
        version: &str,
        install_dir: &Path,
        target_arch: &str,
    ) -> bool {
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

        let is_cross = target_arch != std::env::consts::ARCH;
        let use_rosetta = is_cross && target_arch == "x86_64" && rosetta_available();
        // Only the QEMU cross-arch fallback needs `--arch` (and a matching guest
        // agent). The Rosetta path runs a native aarch64 VZ VM.
        let arch_flag = if is_cross && !use_rosetta {
            Some(lima_arch)
        } else {
            None
        };

        // The QEMU fallback runs a foreign-arch VM, which needs the matching
        // guest agent. Sourcing it from the pinned Lima version keeps host and
        // guest agent in lockstep. A failure here is not fatal: the build drives
        // the guest only over SSH (`limactl shell`), and `ensure_lima_instance`
        // treats SSH reachability, not guest-agent health, as readiness.
        if arch_flag.is_some() {
            let lima_share = lima
                .limactl
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("share/lima");
            if !ensure_cross_guest_agent(&lima_share, lima_arch) {
                println!(
                    "cargo:warning=Could not provision the pinned {} guest agent; \
                     falling back to SSH-only readiness (cross-arch VM start may be slower)",
                    lima_arch
                );
            }
        }

        if !ensure_lima_instance(lima, LIMA_TEMPLATE, arch_flag, use_rosetta) {
            println!(
                "cargo:warning=Could not ensure a running Lima instance for apptainer source build"
            );
            return false;
        }

        let strategy = if use_rosetta {
            "an amd64 chroot under Rosetta (fast)"
        } else if is_cross {
            "QEMU full-system emulation (slow)"
        } else {
            "a native VM"
        };
        println!(
            "cargo:warning=Building apptainer {} for {} via {} (this may take several minutes)...",
            version, target_arch, strategy
        );

        let guest_install_dir = GUEST_APPTAINER_DIR;
        let build_script = if use_rosetta {
            rosetta_build_script(version, guest_install_dir)
        } else {
            native_build_script(version, guest_install_dir)
        };

        let label = format!("apptainer-build-{}", target_arch);
        let mut cmd = lima.lima_command();
        cmd.args(["shell", lima.instance, "--", "bash", "-c", &build_script]);
        if !build_helpers::run_command_streaming(&mut cmd, &label).success {
            return false;
        }

        copy_lima_result_to_host(lima, guest_install_dir, install_dir)
    }

    /// The apptainer build commands shared by every strategy: download + verify
    /// the source, configure, compile, install to `install_dir`. Runs without
    /// `sudo` (callers supply root where the environment needs it), and is
    /// embedded verbatim into a quoted `bash` here-doc on the Rosetta path, so
    /// its `$(nproc)` and quoting must survive unexpanded.
    fn apptainer_compile_steps(version: &str, install_dir: &str) -> String {
        format!(
            r#"cd /tmp
rm -rf apptainer-{version} apptainer-{version}.tar.gz {install_dir}
echo "=== Downloading apptainer {version} source ==="
curl -fsSL https://github.com/apptainer/apptainer/releases/download/v{version}/apptainer-{version}.tar.gz -o apptainer-{version}.tar.gz
echo "=== Verifying apptainer source tarball SHA-256 ==="
echo "{sha}  apptainer-{version}.tar.gz" | sha256sum -c -
tar -xzf apptainer-{version}.tar.gz
cd apptainer-{version}
echo "{version}" > VERSION
echo "=== Configuring apptainer ==="
./mconfig --without-suid --prefix={install_dir}
echo "=== Compiling apptainer ==="
make -C builddir -j"$(nproc)"
echo "=== Installing apptainer ==="
make -C builddir install
rm -rf /tmp/apptainer-{version} /tmp/apptainer-{version}.tar.gz"#,
            version = version,
            install_dir = install_dir,
            sha = APPTAINER_SHA256,
        )
    }

    /// Build script for a native or QEMU-emulated VM: install deps in the full
    /// guest (waiting out cloud-init's apt lock), then run the shared steps.
    fn native_build_script(version: &str, install_dir: &str) -> String {
        format!(
            r#"set -eu
echo "=== Waiting for apt lock ==="
while sudo fuser /var/lib/apt/lists/lock /var/lib/dpkg/lock /var/lib/dpkg/lock-frontend >/dev/null 2>&1; do sleep 2; done
echo "=== Installing build dependencies ==="
sudo apt-get update -qq
sudo apt-get install -y -qq {deps}
{compile}
echo "=== Apptainer build complete ==="
"#,
            deps = APPTAINER_BUILD_DEPS,
            compile = apptainer_compile_steps(version, install_dir),
        )
    }

    /// Build script for the Rosetta path: unpack a pinned amd64 rootfs, bind the
    /// kernel filesystems, then run the shared steps inside an amd64 `chroot`
    /// where every binary executes via Rosetta. The result is moved out of the
    /// chroot to the guest path the copy-back expects and chowned to the Lima
    /// user so the (non-root) tar pipe can read it.
    fn rosetta_build_script(version: &str, install_dir: &str) -> String {
        format!(
            r#"set -eu
ROOT=/amd64
echo "=== Preparing pinned amd64 rootfs (Rosetta) ==="
sudo umount -R -l "$ROOT" 2>/dev/null || true
sudo rm -rf "$ROOT"
sudo mkdir -p "$ROOT"
curl -fsSL {rootfs_url} -o /tmp/amd64base.tar.gz
echo "=== Verifying amd64 rootfs SHA-256 ==="
echo "{rootfs_sha}  /tmp/amd64base.tar.gz" | sha256sum -c -
sudo tar -xzf /tmp/amd64base.tar.gz -C "$ROOT"
sudo mount -t proc proc "$ROOT/proc"
sudo mount --rbind /sys "$ROOT/sys" && sudo mount --make-rslave "$ROOT/sys"
sudo mount --rbind /dev "$ROOT/dev" && sudo mount --make-rslave "$ROOT/dev"
sudo mount -t devpts devpts "$ROOT/dev/pts" 2>/dev/null || true
sudo cp /etc/resolv.conf "$ROOT/etc/resolv.conf"
sudo tee "$ROOT/peppy-apptainer-build.sh" >/dev/null <<'PEPPY_BUILD_EOF'
set -eu
export DEBIAN_FRONTEND=noninteractive
echo "=== Installing build dependencies (amd64, via Rosetta) ==="
apt-get update -qq
apt-get install -y -qq {deps}
{compile}
echo "=== Apptainer build complete ==="
PEPPY_BUILD_EOF
echo "=== Building apptainer in amd64 chroot (Rosetta) ==="
sudo chroot "$ROOT" bash /peppy-apptainer-build.sh
echo "=== Exporting result from chroot ==="
sudo rm -rf {install_dir}
sudo mkdir -p "$(dirname {install_dir})"
sudo cp -a "$ROOT{install_dir}" {install_dir}
sudo chown -R "$(id -u):$(id -g)" {install_dir}
sudo umount -R -l "$ROOT" 2>/dev/null || true
echo "=== Apptainer build complete ==="
"#,
            rootfs_url = ubuntu_base_amd64_url(UBUNTU_BASE_VERSION),
            rootfs_sha = UBUNTU_BASE_AMD64_SHA256,
            deps = APPTAINER_BUILD_DEPS,
            compile = apptainer_compile_steps(version, install_dir),
        )
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
    // Main entry point — orchestration
    // -----------------------------------------------------------------------

    fn emit_rerun_directives() {
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-env-changed=PEPPY_APPTAINER_DIR");
        println!("cargo:rerun-if-env-changed=PEPPY_LIMA_DIR");
        println!("cargo:rerun-if-env-changed=PEPPY_CROSS_ARCH");
        println!("cargo:rerun-if-env-changed=PEPPY_NO_ROSETTA");
    }

    fn emit_constant_env_vars() {
        println!("cargo:rustc-env=LIMA_INSTANCE={}", LIMA_INSTANCE);
        println!("cargo:rustc-env=LIMA_TEMPLATE={}", LIMA_TEMPLATE);
        println!("cargo:rustc-env=APPTAINER_VERSION={}", APPTAINER_VERSION);
        println!("cargo:rustc-env=LIMA_VERSION={}", LIMA_VERSION);
        println!("cargo:rustc-env=GOCRYPTFS_VERSION={}", GOCRYPTFS_VERSION);
        println!(
            "cargo:rustc-env=GUEST_APPTAINER_DIR={}",
            GUEST_APPTAINER_DIR
        );
    }

    /// Download and cache Lima, copy to OUT_DIR, emit env vars.
    /// Panics if Lima cannot be downloaded (it's required for macOS builds).
    fn setup_lima(out_dir: &str) -> LimaConfig {
        let lima_cache_dir =
            ensure_lima_cached(LIMA_VERSION, "Darwin", "arm64").unwrap_or_else(|| {
                panic!(
                    "Could not download Lima {}. Lima is required for macOS builds.",
                    LIMA_VERSION
                );
            });

        let lima = LimaConfig {
            limactl: lima_cache_dir.join("bin/limactl"),
            lima_home: get_lima_build_home(),
            instance: LIMA_INSTANCE,
        };

        // Copy Lima installation to OUT_DIR for the crate to reference at compile time.
        // Use a sentinel to skip the copy when the source hasn't changed.
        let out_lima_dir = PathBuf::from(out_dir).join("lima-install");
        let lima_sentinel_path = out_lima_dir.join(".copy-source");
        let lima_sentinel_content = format!("{}", lima_cache_dir.display());
        let lima_needs_copy = !lima_sentinel_path.exists()
            || std::fs::read_to_string(&lima_sentinel_path)
                .map_or(true, |s| s.trim() != lima_sentinel_content.trim());
        if lima_needs_copy {
            if out_lima_dir.exists() {
                std::fs::remove_dir_all(&out_lima_dir).ok();
            }
            if let Err(e) = copy_dir_recursive(&lima_cache_dir, &out_lima_dir) {
                panic!("Failed to copy Lima installation to OUT_DIR: {}", e);
            }
            std::fs::write(&lima_sentinel_path, &lima_sentinel_content).ok();
        }
        println!(
            "cargo:rustc-env=LIMA_INSTALL_DIR={}",
            out_lima_dir.display()
        );
        println!(
            "cargo:rustc-env=LIMA_BUILD_HOME={}",
            lima.lima_home.display()
        );

        lima
    }

    /// Pre-build apptainer for all target architectures via Lima VMs.
    ///
    /// By default only the native architecture is built. Set PEPPY_CROSS_ARCH=1
    /// (used by the release script) to also build for non-native architectures.
    fn build_lima_targets(lima: &LimaConfig) {
        let cross_arch = env::var("PEPPY_CROSS_ARCH").unwrap_or_default() == "1";
        let native_arch: &str = std::env::consts::ARCH;
        let targets: Vec<&str> = if cross_arch {
            vec!["aarch64", "x86_64"]
        } else {
            vec![native_arch]
        };

        for target in &targets {
            let target_cache = apptainer_cache_dir(APPTAINER_VERSION, target);
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
            assert!(
                ensure_gocryptfs_installed(&target_cache, target),
                "Failed to install gocryptfs {} for {}",
                GOCRYPTFS_VERSION,
                target
            );
            write_cache_sentinel(&target_cache, APPTAINER_VERSION);
        }
    }

    /// On Linux inside a Lima VM, the macOS-side cache is accessible at the
    /// same absolute path because Lima mounts the host home directory.
    /// Scan `/Users/*/` for a cached apptainer build and return its path.
    fn find_macos_cache_fallback(version: &str, arch: &str) -> Option<PathBuf> {
        let macos_home = Path::new("/Users");
        if !macos_home.is_dir() {
            return None;
        }
        let pattern = format!("apptainer-{}-{}-nosuid", version, arch);
        for entry in std::fs::read_dir(macos_home).ok()?.flatten() {
            let candidate = entry.path().join(".peppy/tmp").join(&pattern);
            let sentinel = apptainer_cache_sentinel_path(&candidate, version);
            if sentinel.exists() && candidate.join("bin/apptainer").exists() {
                println!(
                    "cargo:warning=Using macOS-side cached apptainer from {:?}",
                    candidate
                );
                return Some(candidate);
            }
        }
        None
    }

    /// Ensure we have a cached apptainer installation for the given arch.
    /// Returns the path to the cache directory containing `bin/apptainer`.
    fn ensure_apptainer_cached(use_lima: bool, arch: &str) -> PathBuf {
        let cache_dir = apptainer_cache_dir(APPTAINER_VERSION, arch);
        let sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
        let cached_bin = cache_dir.join("bin/apptainer");

        // Check if we already have a valid cache.
        if sentinel.exists() && cached_bin.exists() {
            println!(
                "cargo:warning=Using cached apptainer installation from {:?}",
                cache_dir
            );
            // The apptainer cache may pre-date gocryptfs bundling; ensure the
            // binary is present even when we short-circuit the rest of the build.
            assert!(
                ensure_gocryptfs_installed(&cache_dir, arch),
                "Failed to install gocryptfs {} into cached apptainer dir at {:?}",
                GOCRYPTFS_VERSION,
                cache_dir
            );
            return cache_dir;
        }

        // On Linux, try the macOS-side cache (Lima mounts host home dirs).
        if !use_lima && let Some(macos_cache) = find_macos_cache_fallback(APPTAINER_VERSION, arch) {
            force_remove_dir(&cache_dir);
            copy_dir_recursive(&macos_cache, &cache_dir)
                .expect("Failed to copy macOS apptainer cache");
            assert!(
                ensure_gocryptfs_installed(&cache_dir, arch),
                "Failed to install gocryptfs {} into apptainer cache for {}",
                GOCRYPTFS_VERSION,
                arch
            );
            write_cache_sentinel(&cache_dir, APPTAINER_VERSION);
            return cache_dir;
        }

        // No cache available — build from source (Linux host only).
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
            cache_dir.join("bin/apptainer").exists(),
            "Apptainer source build completed but bin/apptainer not found in {:?}",
            cache_dir
        );
        assert!(
            ensure_gocryptfs_installed(&cache_dir, arch),
            "Failed to install gocryptfs {} for {}",
            GOCRYPTFS_VERSION,
            arch
        );
        write_cache_sentinel(&cache_dir, APPTAINER_VERSION);

        cache_dir
    }

    pub fn run() {
        emit_rerun_directives();
        emit_constant_env_vars();

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

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

        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "aarch64".to_string());
        let out_dir = env::var("OUT_DIR").unwrap();

        // Step 1 (macOS only): Download and cache Lima, pre-build apptainer via VMs
        if use_lima {
            let lima = setup_lima(&out_dir);
            build_lima_targets(&lima);
        }

        // Step 2: Ensure apptainer is cached (from Lima pre-build, macOS fallback, or local build)
        let cache_dir = ensure_apptainer_cached(use_lima, &arch);

        // Track the cache sentinel so Cargo re-runs build.rs if ~/.peppy is deleted.
        let cache_sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
        println!("cargo:rerun-if-changed={}", cache_sentinel.display());

        // Step 3: Copy apptainer installation to OUT_DIR for release packaging.
        // Use a sentinel to skip the copy when the source hasn't changed,
        // avoiding mtime bumps that trigger unnecessary recompilation.
        let out_install_dir = PathBuf::from(&out_dir).join("apptainer-install");
        let sentinel_path = out_install_dir.join(".copy-source");
        let sentinel_content = format!("{}\ngocryptfs={}", cache_dir.display(), GOCRYPTFS_VERSION);
        let needs_copy = !sentinel_path.exists()
            || std::fs::read_to_string(&sentinel_path)
                .map_or(true, |s| s.trim() != sentinel_content.trim());
        if needs_copy {
            if out_install_dir.exists() {
                std::fs::remove_dir_all(&out_install_dir).ok();
            }
            copy_dir_recursive(&cache_dir, &out_install_dir).unwrap_or_else(|e| {
                panic!("Failed to copy apptainer installation to OUT_DIR: {}", e)
            });
            std::fs::write(&sentinel_path, &sentinel_content).ok();
        }

        println!(
            "cargo:rustc-env=APPTAINER_INSTALL_DIR={}",
            cache_dir.display()
        );
    }
}

fn main() {
    apptainer_build::run();
}
