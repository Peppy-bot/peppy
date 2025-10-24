mod capnp_build {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    // Version tags for external binaries (should match Cargo.toml dependencies where applicable)
    const CAPNP_VERSION: &str = "1.2.0";

    fn get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
        let temp_dir = env::temp_dir();
        let cache_dir = temp_dir.join(format!("{cache_suffix}-peppy-cache"));

        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
        }

        cache_dir
    }

    fn run_command(command: &mut Command, description: &str) -> bool {
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

    pub fn build_capnp(release_tag: &str) {
        if env::var("CARGO_FEATURE_BUILD_CAPNP").is_err() {
            return;
        }

        println!("cargo:rerun-if-changed=build.rs");

        let cache_dir = get_temp_cache_dir("capnp");
        let cache_key = format!("capnp-{release_tag}");
        let cached_capnp_path = cache_dir.join(&cache_key);

        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is not set"));
        let capnp_binary_path = out_dir.join("capnp");

        if cached_capnp_path.exists() {
            if let Err(err) = fs::copy(&cached_capnp_path, &capnp_binary_path) {
                println!("cargo:warning=Failed to copy cached capnp binary: {err}");
                return;
            }
        } else {
            println!("cargo:warning=Building capnp binary from source...");

            let source_dir = cache_dir.join("capnp-src");
            if source_dir.exists() {
                let _ = fs::remove_dir_all(&source_dir);
            }

            let git_tag = if release_tag.starts_with('v') {
                release_tag.to_string()
            } else {
                format!("v{release_tag}")
            };

            let mut clone = Command::new("git");
            clone
                .arg("clone")
                .arg("--depth")
                .arg("1")
                .arg("--branch")
                .arg(&git_tag)
                .arg("https://github.com/capnproto/capnproto.git")
                .arg(&source_dir);
            if !run_command(&mut clone, "clone capnp repository") {
                return;
            }

            let build_dir = source_dir.join("build");
            let install_dir = source_dir.join("install");

            if build_dir.exists() {
                let _ = fs::remove_dir_all(&build_dir);
            }
            if install_dir.exists() {
                let _ = fs::remove_dir_all(&install_dir);
            }

            let mut configure = Command::new("cmake");
            configure
                .current_dir(&source_dir)
                .arg("-S")
                .arg("c++")
                .arg("-B")
                .arg("build")
                .arg("-DCMAKE_BUILD_TYPE=Release");
            if !run_command(&mut configure, "configure capnp build") {
                return;
            }

            let mut build = Command::new("cmake");
            build
                .current_dir(&source_dir)
                .arg("--build")
                .arg("build")
                .arg("--target")
                .arg("capnp")
                .arg("--config")
                .arg("Release");
            if !run_command(&mut build, "compile capnp binary") {
                return;
            }

            let mut install = Command::new("cmake");
            install
                .current_dir(&source_dir)
                .arg("--install")
                .arg("build")
                .arg("--prefix")
                .arg(&install_dir);
            if !run_command(&mut install, "install capnp binary") {
                return;
            }

            #[cfg(windows)]
            let built_binary = {
                let mut path = install_dir.join("bin").join("capnp");
                path.set_extension("exe");
                path
            };
            #[cfg(not(windows))]
            let built_binary = install_dir.join("bin").join("capnp");

            if !built_binary.exists() {
                println!(
                    "cargo:warning=Capnp binary not found at expected location: {:?}",
                    built_binary
                );
                return;
            }

            if let Err(err) = fs::copy(&built_binary, &cached_capnp_path) {
                println!("cargo:warning=Failed to cache capnp binary: {err}");
                return;
            }

            if let Err(err) = fs::copy(&built_binary, &capnp_binary_path) {
                println!("cargo:warning=Failed to copy capnp binary to OUT_DIR: {err}");
                return;
            }

            let _ = fs::remove_dir_all(&source_dir);
        }

        println!(
            "cargo:rustc-env=CAPNP_BINARY_PATH={}",
            capnp_binary_path.display()
        );
    }

    pub fn run() {
        build_capnp(CAPNP_VERSION);
    }
}

fn main() {
    capnp_build::run();
}
