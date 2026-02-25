#[cfg(feature = "zenoh")]
mod zenoh_build {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn find_cargo_lock(start_dir: &Path) -> Option<PathBuf> {
        let mut current = Some(start_dir);
        while let Some(dir) = current {
            let candidate = dir.join("Cargo.lock");
            if candidate.exists() {
                return Some(candidate);
            }
            current = dir.parent();
        }
        None
    }

    fn parse_version_value(value: &str) -> Option<String> {
        let value = value.trim();
        let value = value.strip_prefix('"')?;
        let end = value.find('"')?;
        let version = &value[..end];
        if version.is_empty() {
            None
        } else {
            Some(version.to_string())
        }
    }

    fn parse_zenoh_version_from_lock(content: &str) -> Option<String> {
        let mut in_zenoh_package = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "[[package]]" {
                in_zenoh_package = false;
            } else if trimmed == r#"name = "zenoh""# {
                in_zenoh_package = true;
            } else if in_zenoh_package && trimmed.starts_with("version = ") {
                let value = trimmed.trim_start_matches("version = ");
                return parse_version_value(value);
            }
        }
        None
    }

    fn extract_version_from_inline_table(value: &str) -> Option<String> {
        let version_key = "version";
        let pos = value.find(version_key)?;
        let after = &value[pos + version_key.len()..];
        let (_, rhs) = after.split_once('=')?;
        parse_version_value(rhs)
    }

    fn parse_zenoh_version_from_manifest(content: &str) -> Option<String> {
        let mut in_dependencies = false;
        let mut in_zenoh_table = false;
        let mut in_zenoh_inline_table = false;

        for line in content.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_dependencies = trimmed == "[dependencies]";
                in_zenoh_table = trimmed == "[dependencies.zenoh]";
                in_zenoh_inline_table = false;
                continue;
            }

            if in_zenoh_table {
                if let Some((key, value)) = trimmed.split_once('=')
                    && key.trim() == "version"
                {
                    return parse_version_value(value);
                }
                continue;
            }

            if in_zenoh_inline_table {
                if let Some((key, value)) = trimmed.split_once('=')
                    && key.trim() == "version"
                {
                    return parse_version_value(value);
                }
                if trimmed.contains('}') {
                    in_zenoh_inline_table = false;
                }
                continue;
            }

            if in_dependencies && trimmed.starts_with("zenoh") {
                let (_, value) = trimmed.split_once('=')?;
                let value = value.trim();
                if value.starts_with('"') {
                    return parse_version_value(value);
                }
                if value.starts_with('{') {
                    if let Some(version) = extract_version_from_inline_table(value) {
                        return Some(version);
                    }
                    if !value.contains('}') {
                        in_zenoh_inline_table = true;
                    }
                }
            }
        }

        None
    }

    fn get_zenoh_version() -> String {
        let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let manifest_path = PathBuf::from(&manifest_dir).join("Cargo.toml");

        if let Some(lockfile_path) = find_cargo_lock(Path::new(&manifest_dir)) {
            match std::fs::read_to_string(&lockfile_path) {
                Ok(content) => {
                    if let Some(version) = parse_zenoh_version_from_lock(&content) {
                        return version;
                    }
                }
                Err(err) => {
                    println!(
                        "cargo:warning=Failed to read Cargo.lock at {}: {}",
                        lockfile_path.display(),
                        err
                    );
                }
            }
        } else {
            println!("cargo:warning=Cargo.lock not found; falling back to Cargo.toml");
        }

        let content =
            std::fs::read_to_string(&manifest_path).expect("Failed to read Cargo.toml file");
        parse_zenoh_version_from_manifest(&content).unwrap_or_else(|| {
            panic!("Could not determine zenoh version in Cargo.lock or Cargo.toml")
        })
    }

    fn get_temp_cache_dir(cache_suffix: &str) -> PathBuf {
        let temp_dir = env::temp_dir();
        let cache_dir = temp_dir.join("peppy-build-cache").join(cache_suffix);

        // Create cache directory if it doesn't exist
        if !cache_dir.exists() {
            std::fs::create_dir_all(&cache_dir).expect("Failed to create cache directory");
        }

        cache_dir
    }

    pub fn build_zenoh(release_tag: &str) {
        // Build zenoh router binary when the build_zenoh feature is enabled
        if env::var("CARGO_FEATURE_BUILD_ZENOH").is_ok() {
            println!("cargo:rerun-if-changed=build.rs");
            println!("cargo:rerun-if-changed=Cargo.toml");
            let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
            if let Some(lockfile_path) = find_cargo_lock(Path::new(&manifest_dir)) {
                println!("cargo:rerun-if-changed={}", lockfile_path.display());
            }

            let profile = env::var("PROFILE").unwrap();
            let is_release = profile == "release";

            // Use named temp directory for persistent cache
            let cache_dir = get_temp_cache_dir("zenoh");
            let cached_zenoh_path = cache_dir.join(format!("zenohd-{}-{}", release_tag, profile));

            // Always copy to OUT_DIR for runtime access
            let out_dir = env::var("OUT_DIR").unwrap();
            let zenoh_binary_path = format!("{}/zenohd", out_dir);

            // Check if zenohd is already cached
            if cached_zenoh_path.exists() {
                println!(
                    "cargo:warning=Using cached zenohd binary from {:?}",
                    cached_zenoh_path
                );

                // Copy cached binary to OUT_DIR
                std::fs::copy(&cached_zenoh_path, &zenoh_binary_path)
                    .expect("Failed to copy cached zenohd binary");
            } else {
                println!("cargo:warning=Building zenohd binary from source...");

                // Build in a temporary directory within cache
                let build_dir = cache_dir.join("zenoh-src");
                if build_dir.exists() {
                    std::fs::remove_dir_all(&build_dir).ok();
                }

                // Clone zenoh repository
                let output = Command::new("git")
                    .args([
                        "clone",
                        "--depth",
                        "1",
                        "--branch",
                        release_tag,
                        "https://github.com/eclipse-zenoh/zenoh",
                        build_dir.to_str().unwrap(),
                    ])
                    .output()
                    .expect("Failed to execute git clone");

                if !output.status.success() {
                    println!(
                        "cargo:warning=Failed to clone zenoh repository: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    return;
                }

                // Build zenohd in its own cloned directory. Remove CARGO_TARGET_DIR
                // so the binary lands at build_dir/target/{profile}/ as expected.
                let mut cmd = Command::new("cargo");
                cmd.current_dir(&build_dir).env_remove("CARGO_TARGET_DIR");
                if is_release {
                    cmd.args(["build", "--release", "--bin", "zenohd"]);
                } else {
                    cmd.args(["build", "--bin", "zenohd"]);
                }
                let status = cmd.status();

                if status.is_err() || !status.unwrap().success() {
                    println!("cargo:warning=Failed to build zenohd binary");
                    return;
                }

                // Copy to cache with version tag
                let target_subdir = if is_release { "release" } else { "debug" };
                std::fs::copy(
                    build_dir.join(format!("target/{target_subdir}/zenohd")),
                    &cached_zenoh_path,
                )
                .expect("Failed to cache zenohd binary");

                // Copy to OUT_DIR for runtime
                std::fs::copy(&cached_zenoh_path, &zenoh_binary_path)
                    .expect("Failed to copy zenohd binary to OUT_DIR");

                // Clean up build directory
                std::fs::remove_dir_all(&build_dir).ok();
            }

            // Set environment variable for runtime to find the zenohd binary
            println!("cargo:rustc-env=ZENOHD_BINARY_PATH={}", zenoh_binary_path);
        }
    }

    pub fn run() {
        build_zenoh(&get_zenoh_version());
    }
}

fn main() {
    #[cfg(feature = "zenoh")]
    zenoh_build::run();
}
