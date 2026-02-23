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

    fn run_install_script(script_path: &std::path::Path, install_dir: &std::path::Path) -> bool {
        // The script expects an empty directory; ensure it is.
        if install_dir.exists() {
            std::fs::remove_dir_all(install_dir).ok();
        }
        std::fs::create_dir_all(install_dir).expect("Failed to create apptainer install directory");

        let output = Command::new("sh")
            .arg(script_path)
            .args(["-v", APPTAINER_VERSION, "-d", "el9"])
            .arg(install_dir)
            .output();

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

    fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        if !dst.exists() {
            std::fs::create_dir_all(dst)?;
        }
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
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
        if target_os != "linux" {
            println!(
                "cargo:warning=Skipping apptainer build: apptainer is Linux-only (target_os={})",
                target_os
            );
            return;
        }

        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
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
                "cargo:warning=Downloading and installing apptainer {}...",
                APPTAINER_VERSION
            );

            // Download the install script
            let script_path = cache_dir.parent().unwrap().join("install-unprivileged.sh");
            if !download_install_script(&script_path) {
                println!(
                    "cargo:warning=Could not download apptainer install script; apptainer will not be bundled"
                );
                return;
            }

            // Run the install script into the cache directory
            // First remove the cache dir since the script wants an empty or non-existent directory
            if !run_install_script(&script_path, &cache_dir) {
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
    }
}

fn main() {
    #[cfg(feature = "apptainer")]
    apptainer_build::run();
}
