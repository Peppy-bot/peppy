use super::ResolvedNode;
use crate::error::{Error, Result};
use config::consts::NODE_CONFIG_FILE;
use config::{
    node::{NodeConfig, NodeConfigParser},
    peppy_config::{Deployment, HttpRemoteSpec},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tar::Archive;
use ureq::Error as HttpError;
use zstd::stream::read::Decoder;

const CHECKSUM_FILE: &str = ".checksum";

pub fn resolve_remote_url(
    nodes_cache_dir: &Path,
    deployment: &Deployment,
    spec: HttpRemoteSpec,
) -> Result<ResolvedNode> {
    fs::create_dir_all(nodes_cache_dir)?;

    let cache_dir = build_bundle_cache_path(nodes_cache_dir, deployment, &spec.bundle_url);
    let expected_checksum = spec.checksum.as_deref();
    let needs_refresh = should_refresh(&cache_dir, expected_checksum);

    let node = if needs_refresh {
        refresh_bundle(&cache_dir, &spec, deployment, expected_checksum)?
    } else {
        load_manifest(&cache_dir, deployment)?
    };

    Ok(ResolvedNode {
        config: node,
        root_path: cache_dir,
    })
}

fn refresh_bundle(
    cache_dir: &Path,
    spec: &HttpRemoteSpec,
    deployment: &Deployment,
    expected_checksum: Option<&str>,
) -> Result<NodeConfig> {
    let temp_dir_path = cache_dir.with_extension("tmp");
    if temp_dir_path.exists() {
        fs::remove_dir_all(&temp_dir_path)?;
    }
    fs::create_dir_all(&temp_dir_path)?;
    let mut temp_dir = TempDirGuard::new(temp_dir_path);

    let bundle_filename = bundle_file_name(&spec.bundle_url);
    let bundle_path = temp_dir.path().join(bundle_filename);

    download_bundle(&spec.bundle_url, &bundle_path, expected_checksum)?;
    extract_bundle(&bundle_path, temp_dir.path(), &spec.bundle_url)?;
    let _ = fs::remove_file(&bundle_path);

    if let Some(checksum) = expected_checksum {
        fs::write(temp_dir.path().join(CHECKSUM_FILE), checksum)?;
    }

    let node = load_manifest_inner(temp_dir.path(), deployment)?;

    if cache_dir.exists() {
        fs::remove_dir_all(cache_dir)?;
    }
    if let Some(parent) = cache_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(temp_dir.path(), cache_dir)?;
    temp_dir.disarm();

    Ok(node)
}

fn load_manifest(cache_dir: &Path, deployment: &Deployment) -> Result<NodeConfig> {
    load_manifest_inner(cache_dir, deployment)
}

fn load_manifest_inner(dir: &Path, deployment: &Deployment) -> Result<NodeConfig> {
    let manifest_path = dir.join(NODE_CONFIG_FILE);
    if !manifest_path.is_file() {
        return Err(Error::BundleExtraction {
            url: manifest_path.display().to_string(),
            reason: format!("{NODE_CONFIG_FILE} is missing from bundle"),
        });
    }

    let node = NodeConfigParser::from_path(&manifest_path)?;

    if node.manifest.name.as_str() != deployment.name.as_str()
        || node.manifest.tag != deployment.tag
    {
        return Err(Error::NoMatchingNode(
            deployment.name.to_string(),
            deployment.tag.clone(),
        ));
    }

    Ok(node)
}

fn should_refresh(cache_dir: &Path, expected_checksum: Option<&str>) -> bool {
    let manifest_path = cache_dir.join(NODE_CONFIG_FILE);
    if !manifest_path.is_file() {
        return true;
    }

    match expected_checksum {
        Some(expected) => {
            let checksum_path = cache_dir.join(CHECKSUM_FILE);
            match fs::read_to_string(checksum_path) {
                Ok(stored) => stored.trim() != expected.trim(),
                Err(_) => true,
            }
        }
        None => false,
    }
}

fn download_bundle(url: &str, destination: &Path, checksum: Option<&str>) -> Result<()> {
    let parsed_checksum = checksum.map(parse_checksum).transpose()?;

    let response = ureq::get(url).call().map_err(|err| {
        let reason = match err {
            HttpError::StatusCode(code) => format!("unexpected status code {code}"),
            other => other.to_string(),
        };
        Error::HttpDownload {
            url: url.to_string(),
            reason,
        }
    })?;

    let mut reader = response.into_body().into_reader();
    let mut file = fs::File::create(destination)?;
    let mut buffer = [0u8; 8 * 1024];
    let mut sha256 = parsed_checksum
        .as_ref()
        .map(|checksum| match checksum.algorithm {
            ChecksumAlgorithm::Sha256 => Sha256::new(),
        });

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| Error::HttpDownload {
                url: url.to_string(),
                reason: err.to_string(),
            })?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        if let Some(hasher) = sha256.as_mut() {
            hasher.update(&buffer[..read]);
        }
    }
    file.flush()?;

    if let Some(checksum) = parsed_checksum {
        match checksum.algorithm {
            ChecksumAlgorithm::Sha256 => {
                let computed = sha256
                    .expect("sha256 checksum state should exist")
                    .finalize();
                if AsRef::<[u8]>::as_ref(&computed) != checksum.expected.as_slice() {
                    return Err(Error::ChecksumMismatch(url.to_string()));
                }
            }
        }
    }

    Ok(())
}

fn extract_bundle(bundle_path: &Path, destination: &Path, url: &str) -> Result<()> {
    let file = fs::File::open(bundle_path)?;
    let decoder = Decoder::new(file).map_err(|err| Error::BundleExtraction {
        url: url.to_string(),
        reason: err.to_string(),
    })?;
    let mut archive = Archive::new(decoder);
    archive
        .unpack(destination)
        .map_err(|err| Error::BundleExtraction {
            url: url.to_string(),
            reason: err.to_string(),
        })?;
    Ok(())
}

fn parse_checksum(value: &str) -> Result<ParsedChecksum> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidChecksum(
            value.to_string(),
            "checksum cannot be empty".to_string(),
        ));
    }

    let (algorithm, hex_value) = trimmed
        .split_once(':')
        .map(|(alg, hex)| (alg.trim(), hex.trim()))
        .unwrap_or(("sha256", trimmed));

    if hex_value.is_empty() {
        return Err(Error::InvalidChecksum(
            value.to_string(),
            "checksum value cannot be empty".to_string(),
        ));
    }

    let algorithm = match algorithm.to_ascii_lowercase().as_str() {
        "sha256" => ChecksumAlgorithm::Sha256,
        other => {
            return Err(Error::UnsupportedChecksum(other.to_string()));
        }
    };

    let expected = decode_hex(hex_value)
        .map_err(|reason| Error::InvalidChecksum(value.to_string(), reason))?;

    Ok(ParsedChecksum {
        algorithm,
        expected,
    })
}

fn decode_hex(input: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Err("checksum value cannot be empty".to_string());
    }
    if !bytes.len().is_multiple_of(2) {
        return Err("checksum value must have an even length".to_string());
    }

    let mut output = Vec::with_capacity(bytes.len() / 2);
    let iter = bytes.chunks_exact(2);
    for chunk in iter {
        let high = decode_hex_digit(chunk[0])?;
        let low = decode_hex_digit(chunk[1])?;
        output.push((high << 4) | low);
    }

    Ok(output)
}

fn decode_hex_digit(byte: u8) -> std::result::Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(format!("invalid hex digit `{}`", other as char)),
    }
}

fn build_bundle_cache_path(base: &Path, deployment: &Deployment, bundle_url: &str) -> PathBuf {
    let hash = stable_hash_parts(&[
        bundle_url,
        deployment.name.as_str(),
        deployment.tag.as_str(),
    ]);
    let dir_name = bundle_dir_name(bundle_url);
    base.join(format!("{dir_name}-{hash:016x}"))
}

fn bundle_dir_name(bundle_url: &str) -> String {
    let filename = bundle_file_name(bundle_url);
    let mut candidate = filename.as_str();
    for suffix in [".tar.zst", ".tzst", ".zst"] {
        if let Some(stripped) = candidate.strip_suffix(suffix) {
            candidate = stripped;
            break;
        }
    }

    let sanitized: String = candidate
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            '-' | '_' => ch,
            _ => '-',
        })
        .collect();

    let sanitized = sanitized.trim_matches('-');
    let prefix = if sanitized.is_empty() {
        "bundle".to_string()
    } else {
        sanitized.to_string()
    };

    format!("http-{prefix}")
}

fn bundle_file_name(bundle_url: &str) -> String {
    let trimmed = bundle_url.trim();
    let trimmed = trimmed.trim_end_matches('/');
    let segment = trimmed
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("bundle.zst");
    segment
        .split(['?', '#'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("bundle.zst")
        .to_string()
}

fn stable_hash_parts(parts: &[&str]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;

    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            hash ^= u64::from(b'|');
            hash = hash.wrapping_mul(PRIME);
        }

        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }

    hash
}

struct TempDirGuard {
    path: PathBuf,
    disarmed: bool,
}

impl TempDirGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            disarmed: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct ParsedChecksum {
    algorithm: ChecksumAlgorithm,
    expected: Vec<u8>,
}

enum ChecksumAlgorithm {
    Sha256,
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::peppy_config::Name;
    use std::path::Path;

    fn deployment(name: &str, tag: &str) -> Deployment {
        Deployment {
            name: Name::new(name).expect("valid name"),
            source: None,
            tag: tag.to_string(),
            optional: false,
            instances: Vec::new(),
        }
    }

    #[test]
    fn http_cache_path_varies_by_deployment_tag() {
        let base = Path::new("/tmp/nodes");
        let url = "http://localhost:1234/bundles/uvc_camera.tar.zst";

        let v1 = deployment("uvc_camera", "1.0.0");
        let v2 = deployment("uvc_camera", "1.2.3");

        assert_ne!(
            build_bundle_cache_path(base, &v1, url),
            build_bundle_cache_path(base, &v2, url),
        );
    }
}
