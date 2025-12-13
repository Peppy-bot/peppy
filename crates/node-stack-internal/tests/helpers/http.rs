use std::fs;
use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};

pub fn sha256_checksum(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn create_http_bundle(temp_dir: &Path, bundle_name: &str, manifest_content: &str) -> Vec<u8> {
    let manifest_path = temp_dir.join("peppy.json5");
    fs::write(&manifest_path, manifest_content).expect("write manifest");

    let mut tar_data = Vec::new();
    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);
        tar_builder
            .append_path_with_name(&manifest_path, "peppy.json5")
            .expect("append manifest");
        tar_builder.finish().expect("finish tar");
    }

    let bundle_path = temp_dir.join(bundle_name);
    let bundle_file = fs::File::create(&bundle_path).expect("create bundle");
    let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("create zstd encoder");
    encoder
        .write_all(&tar_data)
        .expect("write compressed bundle");
    encoder.finish().expect("finish encoder");

    fs::read(&bundle_path).expect("read bundle")
}
