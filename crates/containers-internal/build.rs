mod apptainer_build {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const APPTAINER_VERSION: &str = "1.4.5";
    const LIMA_VERSION: &str = "2.0.3";
    const LIMA_INSTANCE: &str = "peppy";

    fn install_script_url() -> String {
        format!(
            "https://raw.githubusercontent.com/apptainer/apptainer/v{}/tools/install-unprivileged.sh",
            APPTAINER_VERSION
        )
    }

    /// URL for downloading a Lima release archive.
    fn lima_archive_url(version: &str, os: &str, arch: &str) -> String {
        format!(
            "https://github.com/lima-vm/lima/releases/download/v{version}/lima-{version}-{os}-{arch}.tar.gz"
        )
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
    // Cache directories
    // -----------------------------------------------------------------------

    fn cache_root() -> PathBuf {
        let root = env::temp_dir().join("peppy-build-cache");
        if !root.exists() {
            std::fs::create_dir_all(&root).expect("Failed to create peppy build cache root");
        }
        root
    }

    fn get_apptainer_cache_dir(version: &str, arch: &str) -> PathBuf {
        let cache_dir = cache_root().join(format!("apptainer-{}-{}", version, arch));
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir)
                .expect("Failed to create apptainer cache directory");
        }
        cache_dir
    }

    fn get_lima_cache_dir(version: &str, os: &str, arch: &str) -> PathBuf {
        let cache_dir = cache_root().join(format!("lima-{}-{}-{}", version, os, arch));
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).expect("Failed to create lima cache directory");
        }
        cache_dir
    }

    /// LIMA_HOME for the build-time VM instance.
    ///
    /// Uses `~/.peppy/lima-build/` instead of the temp dir because macOS temp
    /// directories (`/var/folders/.../T/`) are very long and push Unix socket
    /// paths past the 104-character limit.
    fn get_lima_build_home() -> PathBuf {
        let user_home = env::var("HOME").expect("HOME environment variable not set");
        let home = PathBuf::from(user_home).join(".peppy/lima-build");
        if !home.exists() {
            std::fs::create_dir_all(&home).expect("Failed to create lima build data directory");
        }
        home
    }

    // -----------------------------------------------------------------------
    // Lima download and extraction
    // -----------------------------------------------------------------------

    /// Download the Lima release archive to `dest`.
    fn download_lima_archive(dest: &Path, version: &str, os: &str, arch: &str) -> bool {
        let url = lima_archive_url(version, os, arch);
        let status = Command::new("curl")
            .args(["-fsSL", &url, "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => true,
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
            .args(["xzf"])
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
        let cache_dir = get_lima_cache_dir(version, os, arch);
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

        let archive_path = cache_root().join(format!("lima-{}-{}-{}.tar.gz", version, os, arch));
        if !download_lima_archive(&archive_path, version, os, arch) {
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
    // Apptainer install script download
    // -----------------------------------------------------------------------

    fn download_install_script(dest: &Path) -> bool {
        let status = Command::new("curl")
            .args(["-fsSL", &install_script_url(), "-o"])
            .arg(dest)
            .status();

        match status {
            Ok(s) if s.success() => true,
            Ok(s) => {
                println!(
                    "cargo:warning=Failed to download apptainer install script (exit: {})",
                    s
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run curl to download apptainer install script: {}",
                    e
                );
                false
            }
        }
    }

    /// Create a portable POSIX `rpm2cpio` script in `bin_dir` using only standard
    /// tools (`od`, `dd`, `file`).  Based on `scripts/rpm2cpio.sh` from the RPM
    /// project — no perl, no python, no system packages required.
    ///
    /// Returns `true` if the script was written successfully.
    fn create_rpm2cpio_shim(bin_dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        let shim = bin_dir.join("rpm2cpio");
        // Portable rpm2cpio — based on rpm-software-management/rpm scripts/rpm2cpio.sh.
        // Handles both file arguments (`rpm2cpio file.rpm`) and stdin pipes
        // (`curl … | rpm2cpio -`) which the apptainer install script uses.
        // Uses a shell function for extraction to avoid variable-expansion pitfalls
        // with redirections in command strings.
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

    /// Run `install-unprivileged.sh` directly on the host (Linux).
    ///
    /// The upstream script requires `bash`, `rpm2cpio`, and `cpio`. If `rpm2cpio`
    /// is not on PATH we create a portable POSIX shim in a temp directory and
    /// prepend it to PATH — no sudo or system-wide installs needed.
    fn run_install_script_local(script_path: &Path, install_dir: &Path) -> bool {
        // The script expects an empty directory; ensure it is.
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        // Ensure rpm2cpio is available — provide a portable shim if needed.
        let shim_dir = install_dir.parent().unwrap().join("_rpm2cpio_shim");
        let _ = std::fs::create_dir_all(&shim_dir);

        let has_rpm2cpio = Command::new("rpm2cpio")
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();

        let extra_path = if !has_rpm2cpio {
            if !create_rpm2cpio_shim(&shim_dir) {
                println!("cargo:warning=Failed to create rpm2cpio shim");
                return false;
            }
            Some(shim_dir.clone())
        } else {
            None
        };

        let mut cmd = Command::new("bash");
        cmd.arg(script_path)
            .args(["-v", APPTAINER_VERSION, "-d", "el9"])
            .arg(install_dir);

        // Prepend shim directory to PATH if we created one.
        if let Some(ref shim) = extra_path {
            let path = env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{}", shim.display(), path));
        }

        let output = cmd.output();

        // Clean up shim directory.
        if extra_path.is_some() {
            std::fs::remove_dir_all(&shim_dir).ok();
        }

        match output {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Apptainer install script failed (exit: {}): {}",
                    o.status, stderr
                );
                false
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run apptainer install script: {}",
                    e
                );
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lima instance management
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
                    .args(["start", &name_flag, "--tty=false", template])
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

    /// Run `install-unprivileged.sh` inside a Lima VM (macOS).
    ///
    /// Lima provides a Linux guest where the script can execute with `rpm2cpio`
    /// and `cpio`. The flow:
    /// 1. Ensure the peppy Lima instance is running.
    /// 2. Copy the install script into the VM.
    /// 3. Run it inside the VM, installing to a guest-local temp directory.
    /// 4. Copy the resulting directory tree back to the macOS host.
    fn run_install_script_via_lima(
        lima: &LimaConfig,
        script_path: &Path,
        install_dir: &Path,
    ) -> bool {
        // 0) Ensure the Lima instance exists and is running
        if !ensure_lima_instance(lima, "template:default") {
            println!(
                "cargo:warning=Could not ensure a running Lima instance; apptainer will not be bundled"
            );
            return false;
        }

        let guest_script = "/tmp/peppy-apptainer-install.sh";
        let guest_install_dir = "/tmp/peppy-apptainer-install";

        // 1) Copy the script into the VM
        let cp_in = lima
            .lima_command()
            .args([
                "copy",
                &script_path.to_string_lossy(),
                &format!("{}:{guest_script}", lima.instance),
            ])
            .output();
        match &cp_in {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Failed to copy install script into Lima VM (exit: {}): {}",
                    o.status, stderr
                );
                return false;
            }
            Err(e) => {
                println!("cargo:warning=Failed to run limactl copy: {}", e);
                return false;
            }
        }

        // 2) Run the install script inside the VM.
        //    Use `bash` because the upstream script uses bash-isms (e.g. `[[`).
        //    Also ensure rpm2cpio and cpio are available in the guest.
        let run = lima
            .lima_command()
            .args([
                "shell",
                lima.instance,
                "--",
                "bash",
                "-c",
                &format!(
                    "command -v rpm2cpio >/dev/null 2>&1 || sudo apt-get update -qq && sudo apt-get install -y -qq rpm2cpio cpio >/dev/null 2>&1; \
                     rm -rf {guest_install_dir} && bash {guest_script} -v {APPTAINER_VERSION} -d el9 {guest_install_dir}"
                ),
            ])
            .output();
        match &run {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Apptainer install script failed inside Lima VM (exit: {}): {}",
                    o.status, stderr
                );
                return false;
            }
            Err(e) => {
                println!(
                    "cargo:warning=Failed to run install script via limactl shell: {}",
                    e
                );
                return false;
            }
        }

        // 3) Copy the result back to the host via tar pipe.
        //    `limactl copy -r` is unreliable with long or special-character paths,
        //    so we tar in the guest and untar on the host through a pipe.
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        let tar_pipe = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "LIMA_HOME='{}' '{}' shell {} -- tar -cf - -C {} . | tar -xf - -C '{}'",
                lima.lima_home.display(),
                lima.limactl.display(),
                lima.instance,
                guest_install_dir,
                install_dir.display(),
            ))
            .output();
        match &tar_pipe {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                println!(
                    "cargo:warning=Failed to copy apptainer installation from Lima VM (exit: {}): {}",
                    o.status, stderr
                );
                return false;
            }
            Err(e) => {
                println!("cargo:warning=Failed to run tar pipe from Lima VM: {}", e);
                return false;
            }
        }

        // 4) Clean up guest temp files
        let _ = lima
            .lima_command()
            .args([
                "shell",
                lima.instance,
                "--",
                "rm",
                "-rf",
                guest_script,
                guest_install_dir,
            ])
            .status();

        true
    }

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

    pub fn run() {
        println!("cargo:rerun-if-changed=build.rs");

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
                    println!(
                        "cargo:warning=Could not download Lima; apptainer will not be bundled"
                    );
                    return;
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
                println!(
                    "cargo:warning=Failed to copy Lima installation to OUT_DIR: {}",
                    e
                );
                return;
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
        // Step 2: Download and cache apptainer
        // ------------------------------------------------------------------
        let cache_dir = get_apptainer_cache_dir(APPTAINER_VERSION, &arch);
        let out_install_dir = PathBuf::from(&out_dir).join("apptainer-install");

        // Check if we have a valid cached installation (bin/apptainer must exist)
        let cached_bin = cache_dir.join("bin/apptainer");
        if cached_bin.exists() {
            println!(
                "cargo:warning=Using cached apptainer installation from {:?}",
                cache_dir
            );
        } else {
            println!(
                "cargo:warning=Downloading and installing apptainer {}{}...",
                APPTAINER_VERSION,
                if use_lima { " (via Lima)" } else { "" }
            );

            // Download the install script (curl works on both Linux and macOS)
            let script_path = cache_root().join("install-unprivileged.sh");
            if !download_install_script(&script_path) {
                println!(
                    "cargo:warning=Could not download apptainer install script; apptainer will not be bundled"
                );
                return;
            }

            let success = if let Some(ref lima) = lima_config {
                run_install_script_via_lima(lima, &script_path, &cache_dir)
            } else {
                run_install_script_local(&script_path, &cache_dir)
            };

            if !success {
                println!(
                    "cargo:warning=Could not install apptainer; apptainer will not be bundled"
                );
                return;
            }

            // Verify the install produced a binary
            if !cached_bin.exists() {
                println!(
                    "cargo:warning=Apptainer install completed but bin/apptainer not found in {:?}",
                    cache_dir
                );
                return;
            }

            // Clean up the install script
            std::fs::remove_file(&script_path).ok();
        }

        // Copy cached apptainer installation to OUT_DIR
        if out_install_dir.exists() {
            std::fs::remove_dir_all(&out_install_dir).ok();
        }
        if let Err(e) = copy_dir_recursive(&cache_dir, &out_install_dir) {
            println!(
                "cargo:warning=Failed to copy apptainer installation to OUT_DIR: {}",
                e
            );
            return;
        }

        println!(
            "cargo:rustc-env=APPTAINER_INSTALL_DIR={}",
            out_install_dir.display()
        );
        println!("cargo:rustc-env=APPTAINER_VERSION={}", APPTAINER_VERSION);
        println!("cargo:rustc-env=LIMA_INSTANCE={}", LIMA_INSTANCE);
    }
}

fn main() {
    apptainer_build::run();
}
