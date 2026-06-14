#[cfg(feature = "zenoh")]
mod zenoh_build {
    use std::env;
    use std::path::{Path, PathBuf};

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

    pub fn build_zenoh(release_tag: &str) {
        // Compile the zenoh router from source when the build_zenoh feature is
        // enabled. Building the pinned version with `cargo install` keeps the
        // router in lockstep with the `zenoh` library version resolved for this
        // workspace, cached per machine/version/target like the other build
        // tools, instead of fetching a separate prebuilt binary.
        if env::var("CARGO_FEATURE_BUILD_ZENOH").is_ok() {
            println!("cargo:rerun-if-changed=build.rs");
            println!("cargo:rerun-if-changed=Cargo.toml");

            // The pinned zenoh version is read from the resolved workspace
            // lockfile, so rebuild zenohd when that lockfile changes too, not only
            // when build.rs or Cargo.toml change.
            let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
            if let Some(lockfile_path) = find_cargo_lock(Path::new(&manifest_dir)) {
                println!("cargo:rerun-if-changed={}", lockfile_path.display());
            }

            let target = env::var("TARGET").expect("TARGET not set");
            let cache_dir = build_helpers::cache_dir("zenoh");

            let out_dir = env::var("OUT_DIR").unwrap();
            let zenoh_binary_path = format!("{}/zenohd", out_dir);

            match build_helpers::cargo_install_binary("zenohd", release_tag, &target, &cache_dir) {
                Some(compiled) => {
                    build_helpers::copy_if_changed(&compiled, zenoh_binary_path.as_ref());
                }
                None => panic!(
                    "Failed to compile zenohd {} for target '{}'. \
                     Build zenohd from source with \
                     `cargo build --release --target {}` \
                     from the zenoh {} source tree, then place the built binary next to peppy \
                     or point PEPPY_ZENOHD_PATH to it.",
                    release_tag, target, target, release_tag
                ),
            }

            // Ensure the binary is executable (std::fs::copy preserves the
            // source's mode, but the cache may predate the executable bit).
            build_helpers::set_executable(Path::new(&zenoh_binary_path));

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
