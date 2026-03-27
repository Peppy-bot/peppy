#[cfg(feature = "zenoh")]
mod zenoh_build {
    use std::collections::HashMap;
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

    /// Parses `zenoh-checksums.toml` and returns (version, target→hash map).
    fn parse_checksums(content: &str) -> (String, HashMap<String, String>) {
        let mut version = String::new();
        let mut checksums = HashMap::new();
        let mut in_checksums = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed == "[checksums]" {
                in_checksums = true;
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                if let Some(parsed) = parse_version_value(value) {
                    if in_checksums {
                        checksums.insert(key.to_string(), parsed);
                    } else if key == "version" {
                        version = parsed;
                    }
                }
            }
        }

        (version, checksums)
    }

    pub fn build_zenoh(release_tag: &str) {
        // Download pre-built zenoh router binary when the build_zenoh feature is enabled
        if env::var("CARGO_FEATURE_BUILD_ZENOH").is_ok() {
            let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

            println!("cargo:rerun-if-changed=build.rs");
            println!("cargo:rerun-if-changed=Cargo.toml");
            println!("cargo:rerun-if-changed=zenoh-checksums.toml");

            // Load and validate checksums
            let checksums_path = PathBuf::from(&manifest_dir).join("zenoh-checksums.toml");
            let checksums_content = std::fs::read_to_string(&checksums_path)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", checksums_path.display(), e));
            let (checksums_version, checksums) = parse_checksums(&checksums_content);

            assert_eq!(
                checksums_version, release_tag,
                "zenoh-checksums.toml version ({}) does not match detected zenoh version ({}). \
                 Update zenoh-checksums.toml with hashes for the new version.",
                checksums_version, release_tag
            );

            let target = env::var("TARGET").expect("TARGET not set");

            let cache_dir = build_helpers::cache_dir("zenoh");
            let cached_zenoh_path = cache_dir.join(format!("zenohd-{}-{}", release_tag, target));

            let out_dir = env::var("OUT_DIR").unwrap();
            let zenoh_binary_path = format!("{}/zenohd", out_dir);

            if cached_zenoh_path.exists() {
                println!(
                    "cargo:warning=Using cached zenohd binary from {:?}",
                    cached_zenoh_path
                );
                build_helpers::copy_if_changed(&cached_zenoh_path, zenoh_binary_path.as_ref());
            } else if let Some(expected_hash) = checksums.get(&target) {
                // Download pre-built binary (checksum available for this target)
                let url = format!(
                    "https://github.com/eclipse-zenoh/zenoh/releases/download/{version}/zenoh-{version}-{target}-standalone.zip",
                    version = release_tag,
                    target = target,
                );
                println!("cargo:warning=Downloading zenohd from {}", url);

                let zip_path = cache_dir.join(format!("zenoh-{}-{}.zip", release_tag, target));

                // Download
                let status = Command::new("curl")
                    .args(["-fSL", "-o", zip_path.to_str().unwrap(), &url])
                    .status();

                if status.is_err() || !status.as_ref().unwrap().success() {
                    std::fs::remove_file(&zip_path).ok();
                    panic!(
                        "Failed to download zenohd from {}. \
                         Install zenohd manually and set PEPPY_ZENOHD_PATH instead.",
                        url
                    );
                }

                // Verify SHA256
                if !build_helpers::verify_sha256(&zip_path, expected_hash, "zenohd zip") {
                    std::fs::remove_file(&zip_path).ok();
                    panic!(
                        "SHA256 checksum mismatch for {}! \
                         The downloaded file has been deleted. This may indicate a corrupted download or a tampered release.",
                        zip_path.display(),
                    );
                }

                // Extract only zenohd from the zip
                let extract_dir = cache_dir.join("zenoh-extract");
                std::fs::create_dir_all(&extract_dir).ok();

                let file = std::fs::File::open(&zip_path).expect("Failed to open zenoh zip");
                let mut archive = zip::ZipArchive::new(file).expect("Failed to read zenoh zip");
                let mut entry = archive.by_name("zenohd").expect("zenohd not found in zip");
                let mut buf = Vec::with_capacity(entry.size() as usize);
                std::io::Read::read_to_end(&mut entry, &mut buf)
                    .expect("Failed to read zenohd from zip");
                let extracted_binary = extract_dir.join("zenohd");
                std::fs::write(&extracted_binary, &buf).expect("Failed to write extracted zenohd");

                // Cache the binary
                std::fs::copy(&extracted_binary, &cached_zenoh_path)
                    .expect("Failed to cache zenohd binary");

                // Copy to OUT_DIR
                std::fs::copy(&cached_zenoh_path, &zenoh_binary_path)
                    .expect("Failed to copy zenohd binary to OUT_DIR");

                // Clean up
                std::fs::remove_file(&zip_path).ok();
                std::fs::remove_dir_all(&extract_dir).ok();
            } else {
                // No pre-built binary available — compile from source
                println!(
                    "cargo:warning=No pre-built zenohd binary for target '{}', compiling from source...",
                    target
                );
                match build_helpers::cargo_install_binary(
                    "zenohd",
                    release_tag,
                    &target,
                    &cache_dir,
                ) {
                    Some(compiled) => {
                        std::fs::copy(&compiled, &zenoh_binary_path)
                            .expect("Failed to copy zenohd binary to OUT_DIR");
                    }
                    None => panic!(
                        "Failed to download or compile zenohd {} for target '{}'. \
                         Ensure a Rust toolchain with the {} target is installed.",
                        release_tag, target, target
                    ),
                }
            }

            // Ensure the binary is executable (std::fs::write / std::fs::copy do not
            // preserve the execute bit from the zip archive).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &zenoh_binary_path,
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("Failed to set zenohd executable permission");
            }

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
