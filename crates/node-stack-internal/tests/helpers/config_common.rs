use config::{
    node::{NodeConfig, NodeConfigParser},
    peppy_config::{Deployment, DeploymentNodeSource, PeppyLauncher},
};
use std::io::Write;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Returns a minimal master/root node configuration for tests.
/// The master node is the required root of every NodeStack.
pub fn master_node_config() -> NodeConfig {
    NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "master",
                tag: "1.0.0"
            }
        }"#,
    )
    .expect("parse master node config")
}

pub fn node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> NodeConfig {
    let topics = deps
        .iter()
        .map(|(dep_name, dep_tag)| {
            format!(
                "{{ id: \"{dep_name}_topic\", node: \"{dep_name}\", name: \"{dep_name}_topic\", tag: \"{dep_tag}\" }}",
                dep_name = dep_name,
                dep_tag = dep_tag
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let subscribes_block = if deps.is_empty() {
        String::new()
    } else {
        format!(
            ",
                        subscribes_to: {{
                            topics: [ {topics} ]
                        }}",
            topics = topics
        )
    };

    let content = format!(
        r#"{{
                schema_version: 1,
                manifest: {{ name: "{name}", tag: "{tag}" }},
                interfaces: {{
                    exposes: {{
                        topics: [
                            {{ name: "{name}_topic", qos_profile: "standard" }}
                        ]
                    }}{subscribes}
                }}
            }}"#,
        name = name,
        tag = tag,
        subscribes = subscribes_block
    );

    NodeConfigParser::from_content(&content).expect("parse node config")
}

pub fn deployment(
    name: &str,
    tag: &str,
    source: Option<DeploymentNodeSource>,
    optional: bool,
) -> Deployment {
    Deployment {
        name: config::peppy_config::Name::new(name).unwrap(),
        source,
        tag: tag.to_string(),
        optional,
        instances: Vec::new(),
    }
}

pub fn write_config(path: PathBuf, launcher_config: PeppyLauncher) -> PathBuf {
    let content = serde_json5::to_string(&launcher_config).expect("serialize config");
    fs::create_dir_all(path.parent().expect("dir")).expect("create config directory");
    fs::write(&path, content).expect("write config");
    path
}

pub fn write_config_str(path: PathBuf, content: &str) -> PathBuf {
    fs::create_dir_all(path.parent().expect("dir")).expect("create config directory");
    fs::write(&path, content).expect("write config");
    path
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
