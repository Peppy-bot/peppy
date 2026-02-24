#[cfg(feature = "apptainer")]
mod apptainer_build {
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    const APPTAINER_VERSION: &str = "1.4.5";
    const INSTALL_SCRIPT_URL: &str =
        "https://raw.githubusercontent.com/apptainer/apptainer/main/tools/install-unprivileged.sh";

    fn get_temp_cache_dir(version: &str, arch: &str) -> PathBuf {
        let temp_dir = env::temp_dir();
        let cache_dir = temp_dir.join(format!(
            "apptainer-peppy-cache/apptainer-{}-{}",
            version, arch
        ));

        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir)
                .expect("Failed to create apptainer cache directory");
        }

        cache_dir
    }

    fn download_install_script(dest: &std::path::Path) -> bool {
        let status = Command::new("curl")
            .args(["-fsSL", INSTALL_SCRIPT_URL, "-o"])
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
    fn create_rpm2cpio_shim(bin_dir: &std::path::Path) -> bool {
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
    fn run_install_script_local(
        script_path: &std::path::Path,
        install_dir: &std::path::Path,
    ) -> bool {
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

    /// Ensure a Lima "default" instance exists and is running.
    ///
    /// * If the instance does not exist, create and start it.
    /// * If it exists but is stopped, start it.
    /// * If it is already running, this is a no-op.
    fn ensure_lima_instance() -> bool {
        // Query instance status via JSON output.
        let list_output = Command::new("limactl").args(["list", "--json"]).output();

        let instance_status = match &list_output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // limactl list --json prints one JSON object per line (NDJSON).
                // Look for an object whose "name" is "default" and read its "status".
                stdout
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if line.is_empty() {
                            return None;
                        }
                        // Minimal JSON parsing: look for "name":"default" and grab "status".
                        if line.contains("\"name\":\"default\"")
                            || line.contains("\"name\": \"default\"")
                        {
                            // Extract the status value.
                            if let Some(pos) = line.find("\"status\"") {
                                let after = &line[pos..];
                                // Find the value after the colon.
                                if let Some(colon) = after.find(':') {
                                    let val_part =
                                        after[colon + 1..].trim().trim_start_matches('"');
                                    if let Some(end) = val_part.find('"') {
                                        return Some(val_part[..end].to_string());
                                    }
                                }
                            }
                        }
                        None
                    })
                    .next()
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
                println!("cargo:warning=Starting Lima default instance...");
                let start = Command::new("limactl").args(["start", "default"]).output();
                match start {
                    Ok(o) if o.status.success() => true,
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "cargo:warning=Failed to start Lima default instance (exit: {}): {}",
                            o.status, stderr
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
                    "cargo:warning=Creating Lima default instance (this may take a few minutes on first run)..."
                );
                let create = Command::new("limactl")
                    .args(["start", "default", "--tty=false"])
                    .output();
                match create {
                    Ok(o) if o.status.success() => true,
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!(
                            "cargo:warning=Failed to create Lima default instance (exit: {}): {}",
                            o.status, stderr
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
    /// 1. Ensure the default Lima instance is running.
    /// 2. Copy the install script into the VM.
    /// 3. Run it inside the VM, installing to a guest-local temp directory.
    /// 4. Copy the resulting directory tree back to the macOS host.
    fn run_install_script_via_lima(
        script_path: &std::path::Path,
        install_dir: &std::path::Path,
    ) -> bool {
        // 0) Ensure the Lima default instance exists and is running
        if !ensure_lima_instance() {
            println!(
                "cargo:warning=Could not ensure a running Lima instance; apptainer will not be bundled"
            );
            return false;
        }

        let guest_script = "/tmp/peppy-apptainer-install.sh";
        let guest_install_dir = "/tmp/peppy-apptainer-install";

        // 1) Copy the script into the VM
        let cp_in = Command::new("limactl")
            .args([
                "copy",
                &script_path.to_string_lossy(),
                &format!("default:{guest_script}"),
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
        let run = Command::new("limactl")
            .args([
                "shell",
                "default",
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

        // 3) Copy the result back to the host
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        let cp_out = Command::new("limactl")
            .args([
                "copy",
                "-r",
                &format!("default:{guest_install_dir}/."),
                &install_dir.to_string_lossy(),
            ])
            .output();
        match &cp_out {
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
                println!("cargo:warning=Failed to run limactl copy for result: {}", e);
                return false;
            }
        }

        // 4) Clean up guest temp files
        let _ = Command::new("limactl")
            .args([
                "shell",
                "default",
                "--",
                "rm",
                "-rf",
                guest_script,
                guest_install_dir,
            ])
            .status();

        true
    }

    fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
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
        if env::var("CARGO_FEATURE_BUILD_APPTAINER").is_err() {
            return;
        }

        println!("cargo:rerun-if-changed=build.rs");

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let use_lima = if target_os == "macos" {
            // Apptainer is Linux-only; on macOS it runs inside a Lima VM.
            let has_lima = Command::new("limactl")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if !has_lima {
                panic!(
                    "\n\n\
                     Apptainer requires Lima on macOS.\n\
                     Lima provides a lightweight Linux VM for running apptainer containers.\n\n\
                     Install Lima with:  brew install lima\n\
                     Then re-run the build.\n\n"
                );
            }

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
        let cache_dir = get_temp_cache_dir(APPTAINER_VERSION, &arch);

        let out_dir = env::var("OUT_DIR").unwrap();
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
            let script_path = cache_dir.parent().unwrap().join("install-unprivileged.sh");
            if !download_install_script(&script_path) {
                println!(
                    "cargo:warning=Could not download apptainer install script; apptainer will not be bundled"
                );
                return;
            }

            let success = if use_lima {
                run_install_script_via_lima(&script_path, &cache_dir)
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

        // Copy cached installation to OUT_DIR
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
    }
}

fn main() {
    #[cfg(feature = "apptainer")]
    apptainer_build::run();
}
