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

    const APPTAINER_RELEASE: &str = "3.el9";
    const APPTAINER_X86_64_RPM_SHA256: &str =
        "1aa20c564fe72ad7023ce5eed0df3d941de56220291c028b073d827b6ef693ee";
    const APPTAINER_AARCH64_RPM_SHA256: &str =
        "604ffc47525dabcbfe5f4a19a332a31c8387d9b971e38954bfdcf342d83fb040";

    /// Discover dependency RPMs in `dep_dir` (all `.rpm` files).
    ///
    /// `dep_dir` is the architecture-specific vendor subdirectory (e.g. `vendor/x86_64/`)
    /// which contains only dependency RPMs — the main apptainer RPM is downloaded
    /// separately at build time.
    fn dependency_rpms(dep_dir: &Path) -> Vec<PathBuf> {
        let mut rpms: Vec<PathBuf> = std::fs::read_dir(dep_dir)
            .expect("Failed to read vendor dependency directory")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension().and_then(|e| e.to_str()) == Some("rpm") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
        rpms.sort();
        rpms
    }

    /// Build the RPM filename for a given architecture.
    fn apptainer_rpm_filename(arch: &str) -> String {
        format!(
            "apptainer-{}-{}.{}.rpm",
            APPTAINER_VERSION, APPTAINER_RELEASE, arch
        )
    }

    /// Build the Koji download URL for the main apptainer RPM.
    fn apptainer_rpm_url(version: &str, release: &str, arch: &str) -> String {
        let filename = format!("apptainer-{}-{}.{}.rpm", version, release, arch);
        format!(
            "https://kojipkgs.fedoraproject.org/packages/apptainer/{}/{}/{}/{}",
            version, release, arch, filename
        )
    }

    /// Return the pinned SHA-256 hash for the apptainer RPM of the given architecture.
    fn apptainer_rpm_sha256(arch: &str) -> Option<&'static str> {
        match arch {
            "x86_64" => Some(APPTAINER_X86_64_RPM_SHA256),
            "aarch64" => Some(APPTAINER_AARCH64_RPM_SHA256),
            _ => None,
        }
    }

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
    // Apptainer RPM download and caching
    // -----------------------------------------------------------------------

    /// Download the main apptainer RPM to `dest`, verifying its SHA-256 checksum.
    fn download_apptainer_rpm(
        dest: &Path,
        version: &str,
        release: &str,
        arch: &str,
        expected_sha256: &str,
    ) -> bool {
        let url = apptainer_rpm_url(version, release, arch);
        let status = Command::new("curl")
            .args(["-fsSL", &url, "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => {
                if !build_helpers::verify_sha256(dest, expected_sha256, "Apptainer RPM") {
                    std::fs::remove_file(dest).ok();
                    return false;
                }
                true
            }
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to download Apptainer RPM from {} (exit: {})",
                    url, s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run curl to download Apptainer RPM: {}",
                    e
                );
                false
            }
        }
    }

    /// Download and cache the main apptainer RPM. Returns the path to the
    /// cached RPM file on success, or `None` if no pre-built RPM is available
    /// for this architecture.
    fn ensure_apptainer_rpm_cached(version: &str, release: &str, arch: &str) -> Option<PathBuf> {
        let filename = apptainer_rpm_filename(arch);
        let downloads_dir = build_helpers::cache_dir("downloads");
        let cached_rpm = downloads_dir.join(&filename);

        if cached_rpm.exists()
            && let Some(expected_sha256) = apptainer_rpm_sha256(arch)
        {
            if build_helpers::verify_sha256(&cached_rpm, expected_sha256, "Cached Apptainer RPM") {
                println!(
                    "cargo:warning=Using cached Apptainer RPM from {:?}",
                    cached_rpm
                );
                return Some(cached_rpm);
            }
            println!("cargo:warning=Cached Apptainer RPM failed verification, re-downloading...");
            std::fs::remove_file(&cached_rpm).ok();
        }

        println!(
            "cargo:warning=Downloading Apptainer {} RPM for {}...",
            version, arch
        );

        let expected_sha256 = apptainer_rpm_sha256(arch)?;

        if !download_apptainer_rpm(&cached_rpm, version, release, arch, expected_sha256) {
            return None;
        }

        Some(cached_rpm)
    }

    // -----------------------------------------------------------------------
    // rpm2cpio shim
    // -----------------------------------------------------------------------

    /// Create a portable POSIX `rpm2cpio` script in `bin_dir` using only standard
    /// tools (`od`, `dd`, `file`).  Based on `scripts/rpm2cpio.sh` from the RPM
    /// project — no perl, no python, no system packages required.
    ///
    /// Returns `true` if the script was written successfully.
    fn create_rpm2cpio_shim(bin_dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let shim = bin_dir.join("rpm2cpio");
        let script = r#"#!/bin/sh
pkg="$1"
_tmp=""
if [ "$pkg" = "-" ] || [ -z "$pkg" ]; then
    _tmp=$(mktemp); cat > "$_tmp"; pkg="$_tmp"
fi
if [ ! -e "$pkg" ]; then
    echo "rpm2cpio: no package supplied" >&2
    [ -n "$_tmp" ] && rm -f "$_tmp"; exit 1
fi
leadsize=96
o=$(expr $leadsize + 8)
set -- $(od -j $o -N 8 -t u1 "$pkg")
il=$(expr 256 \* \( 256 \* \( 256 \* $2 + $3 \) + $4 \) + $5)
dl=$(expr 256 \* \( 256 \* \( 256 \* $6 + $7 \) + $8 \) + $9)
sigsize=$(expr 8 + 16 \* $il + $dl)
o=$(expr $o + \( 8 - \( $sigsize \% 8 \) \) \% 8 + $sigsize + 8)
set -- $(od -j $o -N 8 -t u1 "$pkg")
il=$(expr 256 \* \( 256 \* \( 256 \* $2 + $3 \) + $4 \) + $5)
dl=$(expr 256 \* \( 256 \* \( 256 \* $6 + $7 \) + $8 \) + $9)
hdrsize=$(expr 8 + 16 \* $il + $dl)
o=$(expr $o + $hdrsize)
_extract() { dd if="$pkg" ibs="$o" skip=1 2>/dev/null; }
COMPRESSION=$( (_extract | file -) 2>/dev/null )
_rc=0
if echo "$COMPRESSION" | grep -iq gzip; then _extract | gunzip
elif echo "$COMPRESSION" | grep -iq bzip2; then _extract | bunzip2
elif echo "$COMPRESSION" | grep -iq xz; then _extract | unxz
elif echo "$COMPRESSION" | grep -iq zst; then _extract | unzstd
elif echo "$COMPRESSION" | grep -iq cpio; then _extract
else echo "Unrecognized rpm: $pkg" >&2; _rc=1
fi
[ -n "$_tmp" ] && rm -f "$_tmp"; exit $_rc
"#;
        if std::fs::write(&shim, script).is_err() {
            return false;
        }
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).is_ok()
    }

    // -----------------------------------------------------------------------
    // Local RPM installation (Linux)
    // -----------------------------------------------------------------------

    /// Install apptainer from downloaded RPM and vendored dependency RPMs.
    ///
    /// 1. Creates the `{install_dir}/{arch}/` directory.
    /// 2. Extracts the main apptainer RPM there.
    /// 3. Extracts dependency RPMs into `{install_dir}/{arch}/tmp/`.
    /// 4. Runs the install script to restructure everything.
    fn install_from_local_rpms(
        apptainer_rpm: &Path,
        dep_dir: &Path,
        install_script: &Path,
        install_dir: &Path,
        arch: &str,
    ) -> bool {
        // Start fresh
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        // Create rpm2cpio shim
        let shim_dir = install_dir.parent().unwrap().join("_rpm2cpio_shim");
        let _ = std::fs::create_dir_all(&shim_dir);
        if !create_rpm2cpio_shim(&shim_dir) {
            println!("cargo:warning=Failed to create rpm2cpio shim");
            return false;
        }
        let rpm2cpio = shim_dir.join("rpm2cpio");

        let arch_dir = install_dir.join(arch);
        std::fs::create_dir_all(&arch_dir).expect("Failed to create arch directory");

        // Extract the main apptainer RPM into {arch_dir}/
        if !apptainer_rpm.exists() {
            println!(
                "cargo:warning=Apptainer RPM not found at {:?}",
                apptainer_rpm
            );
            return false;
        }
        if !extract_rpm(&rpm2cpio, apptainer_rpm, &arch_dir) {
            println!("cargo:warning=Failed to extract apptainer RPM");
            return false;
        }

        // Extract dependency RPMs into {arch_dir}/tmp/
        let tmp_dir = arch_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir)
            .expect("Failed to create tmp directory for dependency RPMs");
        let dep_rpms = dependency_rpms(dep_dir);
        for rpm_path in &dep_rpms {
            if !extract_rpm(&rpm2cpio, rpm_path, &tmp_dir) {
                println!(
                    "cargo:warning=Failed to extract dependency RPM: {:?}",
                    rpm_path.file_name().unwrap()
                );
                return false;
            }
        }

        // Run install script to restructure the extracted files
        if !install_script.exists() {
            println!(
                "cargo:warning=Install script not found at {:?}",
                install_script
            );
            return false;
        }

        let output = Command::new("bash")
            .arg(install_script)
            .arg(install_dir)
            .arg(arch)
            .output();

        // Clean up shim directory
        std::fs::remove_dir_all(&shim_dir).ok();

        match output {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                println!(
                    "cargo:warning=install-unprivileged.sh failed (exit: {}): {} {}",
                    o.status, stderr, stdout
                );
                false
            }
            Err(e) => {
                println!("cargo:warning=Failed to run install-unprivileged.sh: {}", e);
                false
            }
        }
    }

    /// Extract a single RPM file into `dest_dir` using rpm2cpio + cpio.
    fn extract_rpm(rpm2cpio: &Path, rpm_path: &Path, dest_dir: &Path) -> bool {
        let output = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "'{}' '{}' | cpio -idum --quiet 2>&1",
                rpm2cpio.display(),
                rpm_path.display()
            ))
            .current_dir(dest_dir)
            .output();

        match output {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let stdout = String::from_utf8_lossy(&o.stdout);
                println!(
                    "cargo:warning=rpm2cpio+cpio failed for {:?} (exit: {}): {} {}",
                    rpm_path, o.status, stderr, stdout
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run rpm2cpio+cpio for {:?}: {}",
                    rpm_path, e
                );
                false
            }
        }
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

    /// Install apptainer via Lima VM (macOS) from RPMs.
    ///
    /// Copies the downloaded RPM and vendored dependency RPMs into the Lima
    /// guest, extracts them there, and copies the result back to the host.
    fn install_via_lima(
        lima: &LimaConfig,
        apptainer_rpm: &Path,
        dep_dir: &Path,
        install_script: &Path,
        install_dir: &Path,
        arch: &str,
    ) -> bool {
        if !ensure_lima_instance(lima, LIMA_TEMPLATE) {
            println!(
                "cargo:warning=Could not ensure a running Lima instance; apptainer will not be bundled"
            );
            return false;
        }

        // Disable AppArmor user namespace restriction in the guest (Ubuntu 24.04+ default).
        let userns_fix = lima
            .lima_command()
            .args([
                "shell",
                lima.instance,
                "--",
                "sudo",
                "sh",
                "-c",
                "if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && \
                 [ \"$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)\" = '1' ]; then \
                   echo 'kernel.apparmor_restrict_unprivileged_userns=0' > /etc/sysctl.d/99-userns.conf && \
                   sysctl --system; \
                 fi",
            ])
            .output();
        match &userns_fix {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Failed to disable AppArmor userns restriction in Lima guest (exit: {}): {}",
                    o.status, stderr
                );
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run AppArmor userns fix in Lima guest: {}",
                    e
                );
            }
        }

        let guest_vendor = "/tmp/peppy-vendor";
        let guest_install_dir = "/tmp/peppy-apptainer-install";

        // 1) Clean up any previous guest state
        let _ = lima
            .lima_command()
            .args([
                "shell",
                lima.instance,
                "--",
                "rm",
                "-rf",
                guest_vendor,
                guest_install_dir,
            ])
            .status();

        // 2) Create guest vendor directory and copy RPMs + post-install script
        let _ = lima
            .lima_command()
            .args(["shell", lima.instance, "--", "mkdir", "-p", guest_vendor])
            .status();

        let rpm_filename = apptainer_rpm
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        // Copy install script, main RPM, and dependency RPMs
        let mut files_to_copy: Vec<PathBuf> = vec![install_script.to_path_buf()];
        files_to_copy.push(apptainer_rpm.to_path_buf());
        files_to_copy.extend(dependency_rpms(dep_dir));

        for file in &files_to_copy {
            let cp = lima
                .lima_command()
                .args([
                    "copy",
                    &file.to_string_lossy(),
                    &format!(
                        "{}:{}/{}",
                        lima.instance,
                        guest_vendor,
                        file.file_name().unwrap().to_string_lossy()
                    ),
                ])
                .output();
            match &cp {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    println!(
                        "cargo:warning=Failed to copy {:?} into Lima VM (exit: {}): {}",
                        file.file_name().unwrap(),
                        o.status,
                        stderr
                    );
                    return false;
                }
                Err(e) => {
                    println!("cargo:warning=Failed to run limactl copy: {}", e);
                    return false;
                }
            }
        }

        // 3) Install rpm2cpio in the guest if needed, then extract RPMs and post-process
        let install_script_name = install_script
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let install_cmd = format!(
            r#"set -e
command -v rpm2cpio >/dev/null 2>&1 || (sudo apt-get update -qq && sudo apt-get install -y -qq rpm2cpio cpio)
mkdir -p {guest_install_dir}/{arch}
cd {guest_install_dir}/{arch}
rpm2cpio {guest_vendor}/{rpm_filename} | cpio -idum --quiet
mkdir -p tmp
cd tmp
for rpm in {guest_vendor}/*.rpm; do
    [ "$(basename "$rpm")" = "{rpm_filename}" ] && continue
    rpm2cpio "$rpm" | cpio -idum --quiet
done
cd {guest_install_dir}/{arch}
bash {guest_vendor}/{install_script_name} {guest_install_dir} {arch}"#,
            guest_install_dir = guest_install_dir,
            guest_vendor = guest_vendor,
            arch = arch,
            rpm_filename = rpm_filename,
            install_script_name = install_script_name,
        );

        let run = lima
            .lima_command()
            .args(["shell", lima.instance, "--", "bash", "-c", &install_cmd])
            .output();
        match &run {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Apptainer installation failed inside Lima VM (exit: {}): {}",
                    o.status, stderr
                );
                return false;
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run install via limactl shell: {}",
                    e
                );
                return false;
            }
        }

        // 4) Copy the result back to the host via tar pipe
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
    // Build apptainer from source (fallback when no pre-built RPM exists)
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

        // Configure: ./mconfig --without-suid --prefix=<install_dir>
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

        let guest_install_dir = "/tmp/peppy-apptainer-source-install";
        let build_script = format!(
            r#"set -eu
sudo apt-get update -qq
sudo apt-get install -y -qq golang-go libseccomp-dev make gcc pkg-config squashfs-tools cryptsetup > /dev/null 2>&1
cd /tmp
rm -rf apptainer-{version} apptainer-{version}.tar.gz {guest_install_dir}
curl -fsSL https://github.com/apptainer/apptainer/releases/download/v{version}/apptainer-{version}.tar.gz -o apptainer-{version}.tar.gz
tar -xzf apptainer-{version}.tar.gz
cd apptainer-{version}
./mconfig --without-suid --prefix={guest_install_dir}
make -C builddir -j
make -C builddir install
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
        println!(
            "cargo:rerun-if-changed=vendor/apptainer-{}-install-unprivileged.sh",
            APPTAINER_VERSION
        );
        println!("cargo:rerun-if-changed=vendor/x86_64/");
        println!("cargo:rerun-if-changed=vendor/aarch64/");
        println!("cargo:rerun-if-env-changed=PEPPY_APPTAINER_DIR");
        println!("cargo:rerun-if-env-changed=PEPPY_LIMA_DIR");

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

        println!("cargo:rustc-env=LIMA_INSTANCE={}", LIMA_INSTANCE);
        println!("cargo:rustc-env=LIMA_TEMPLATE={}", LIMA_TEMPLATE);
        println!("cargo:rustc-env=APPTAINER_VERSION={}", APPTAINER_VERSION);
        println!("cargo:rustc-env=LIMA_VERSION={}", LIMA_VERSION);

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
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let vendor_base = PathBuf::from(&manifest_dir).join("vendor");
        let dep_dir = vendor_base.join(&arch);
        let install_script = vendor_base.join(format!(
            "apptainer-{}-install-unprivileged.sh",
            APPTAINER_VERSION
        ));

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
        // Step 2: Install apptainer (from RPM or built from source)
        // ------------------------------------------------------------------
        let cache_dir =
            build_helpers::cache_dir(&format!("apptainer-{}-{}", APPTAINER_VERSION, &arch));
        let out_install_dir = PathBuf::from(&out_dir).join("apptainer-install");

        // Check if we have a fully completed cached installation.
        // `bin/apptainer` can exist from a partial install, so require a sentinel
        // written only after successful installation.
        let cached_bin = cache_dir.join("bin/apptainer");
        let cache_sentinel = apptainer_cache_sentinel_path(&cache_dir, APPTAINER_VERSION);
        if cache_sentinel.exists() && cached_bin.exists() {
            println!(
                "cargo:warning=Using cached apptainer installation from {:?}",
                cache_dir
            );
        } else if let Some(apptainer_rpm) =
            ensure_apptainer_rpm_cached(APPTAINER_VERSION, APPTAINER_RELEASE, &arch)
        {
            // Pre-built RPM available — install from RPMs
            println!(
                "cargo:warning=Installing apptainer {} from RPMs{}...",
                APPTAINER_VERSION,
                if use_lima { " (via Lima)" } else { "" }
            );

            let success = if let Some(ref lima) = lima_config {
                install_via_lima(
                    lima,
                    &apptainer_rpm,
                    &dep_dir,
                    &install_script,
                    &cache_dir,
                    &arch,
                )
            } else {
                install_from_local_rpms(
                    &apptainer_rpm,
                    &dep_dir,
                    &install_script,
                    &cache_dir,
                    &arch,
                )
            };

            assert!(
                success,
                "Failed to install apptainer {} from RPMs for {}",
                APPTAINER_VERSION, arch
            );

            assert!(
                cached_bin.exists(),
                "Apptainer install completed but bin/apptainer not found in {:?}",
                cache_dir
            );

            std::fs::write(&cache_sentinel, format!("version={}\n", APPTAINER_VERSION))
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to write apptainer cache sentinel {:?}: {}",
                        cache_sentinel, e
                    )
                });
        } else {
            // No pre-built RPM for this architecture — build from source
            println!(
                "cargo:warning=No pre-built apptainer RPM for {}, building from source...",
                arch
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

        // ------------------------------------------------------------------
        // Step 3: Copy cached apptainer installation to OUT_DIR
        // ------------------------------------------------------------------
        if out_install_dir.exists() {
            std::fs::remove_dir_all(&out_install_dir).ok();
        }
        copy_dir_recursive(&cache_dir, &out_install_dir)
            .unwrap_or_else(|e| panic!("Failed to copy apptainer installation to OUT_DIR: {}", e));

        println!(
            "cargo:rustc-env=APPTAINER_INSTALL_DIR={}",
            out_install_dir.display()
        );
    }
}

fn main() {
    apptainer_build::run();
}
