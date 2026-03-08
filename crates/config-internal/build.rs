mod capnp_build {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;

    // Version tags for external binaries (should match Cargo.toml dependencies where applicable)
    const CAPNP_VERSION: &str = "1.2.0";

    /// Build capnp from source using git clone + cmake.
    /// Panics if cmake or git are not available.
    fn build_capnp_from_source(release_tag: &str) {
        println!("cargo:rerun-if-changed=build.rs");

        let profile = env::var("PROFILE").unwrap();
        let cmake_build_type = if profile == "release" {
            "Release"
        } else {
            "Debug"
        };

        let cache_dir = build_helpers::cache_dir("capnp");
        let cache_key = format!("capnp-{release_tag}-{profile}");
        let cached_capnp_path = cache_dir.join(&cache_key);

        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is not set"));
        let capnp_binary_path = out_dir.join("capnp");

        if cached_capnp_path.exists() {
            fs::copy(&cached_capnp_path, &capnp_binary_path)
                .expect("Failed to copy cached capnp binary");
        } else {
            println!("cargo:warning=Building capnp binary from source (requires cmake and git)...");

            let source_dir = cache_dir.join("capnp-src");
            if source_dir.exists() {
                let _ = fs::remove_dir_all(&source_dir);
            }

            let git_tag = if release_tag.starts_with('v') {
                release_tag.to_string()
            } else {
                format!("v{release_tag}")
            };

            assert!(
                build_helpers::run_command(
                    Command::new("git")
                        .arg("clone")
                        .arg("--depth")
                        .arg("1")
                        .arg("--branch")
                        .arg(&git_tag)
                        .arg("https://github.com/capnproto/capnproto.git")
                        .arg(&source_dir),
                    "clone capnp repository"
                ),
                "Failed to clone capnproto. Ensure git is installed."
            );

            let build_dir = source_dir.join("build");
            let install_dir = source_dir.join("install");

            if build_dir.exists() {
                let _ = fs::remove_dir_all(&build_dir);
            }
            if install_dir.exists() {
                let _ = fs::remove_dir_all(&install_dir);
            }

            assert!(
                build_helpers::run_command(
                    Command::new("cmake")
                        .current_dir(&source_dir)
                        .arg("-S")
                        .arg("c++")
                        .arg("-B")
                        .arg("build")
                        .arg(format!("-DCMAKE_BUILD_TYPE={cmake_build_type}")),
                    "configure capnp build"
                ),
                "Failed to configure capnp. Ensure cmake is installed."
            );

            assert!(
                build_helpers::run_command(
                    Command::new("cmake")
                        .current_dir(&source_dir)
                        .arg("--build")
                        .arg("build")
                        .arg("--target")
                        .arg("capnp")
                        .arg("--config")
                        .arg(cmake_build_type),
                    "compile capnp binary"
                ),
                "Failed to compile capnp."
            );

            assert!(
                build_helpers::run_command(
                    Command::new("cmake")
                        .current_dir(&source_dir)
                        .arg("--install")
                        .arg("build")
                        .arg("--prefix")
                        .arg(&install_dir),
                    "install capnp binary"
                ),
                "Failed to install capnp."
            );

            let built_binary = install_dir.join("bin").join("capnp");
            assert!(
                built_binary.exists(),
                "Capnp binary not found at expected location: {:?}",
                built_binary
            );

            fs::copy(&built_binary, &cached_capnp_path).expect("Failed to cache capnp binary");
            fs::copy(&built_binary, &capnp_binary_path)
                .expect("Failed to copy capnp binary to OUT_DIR");

            let _ = fs::remove_dir_all(&source_dir);
        }

        println!(
            "cargo:rustc-env=CAPNP_BINARY_PATH={}",
            capnp_binary_path.display()
        );
    }

    /// Try to embed a bundled capnp binary for the target platform.
    /// Returns `true` if a bundled binary was found and embedded, `false` otherwise.
    fn try_embed_bundled_capnp() -> bool {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let tools_dir = manifest_dir.join("tools");
        let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

        let binary_name = match (target_os.as_str(), target_arch.as_str()) {
            ("linux", "x86_64") => "capnp_linux_x86_64",
            ("linux", "aarch64") => "capnp_linux_aarch64",
            ("macos", "aarch64") => "capnp_macos_aarch64",
            _ => return false,
        };

        let binary_path = tools_dir.join(binary_name);
        println!("cargo:rerun-if-changed={}", binary_path.display());

        let generated = out_dir.join("embedded_capnp.rs");
        let mut file = fs::File::create(&generated).unwrap();
        writeln!(
            file,
            r#"pub const CAPNP_BINARY: Option<&[u8]> = Some(include_bytes!("{}"));"#,
            binary_path.display()
        )
        .unwrap();

        true
    }

    /// Embed the capnp binary built from source (in OUT_DIR) via include_bytes.
    fn embed_built_capnp() {
        let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
        let capnp_binary_path = out_dir.join("capnp");

        let generated = out_dir.join("embedded_capnp.rs");
        let mut file = fs::File::create(&generated).unwrap();

        assert!(
            capnp_binary_path.exists(),
            "Expected capnp binary at {:?} after source build, but not found",
            capnp_binary_path
        );

        writeln!(
            file,
            r#"pub const CAPNP_BINARY: Option<&[u8]> = Some(include_bytes!("{}"));"#,
            capnp_binary_path.display()
        )
        .unwrap();
    }

    pub fn run() {
        if try_embed_bundled_capnp() {
            return;
        }

        // No bundled binary for this platform — build from source
        println!(
            "cargo:warning=No bundled capnp binary for this platform, building from source..."
        );
        build_capnp_from_source(CAPNP_VERSION);
        embed_built_capnp();
    }
}

fn main() {
    capnp_build::run();
}
