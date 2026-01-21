use config::{
    node::{NodeConfig, NodeConfigParser},
    peppy_config::{Deployment, DeploymentInstance, DeploymentNodeSource, Name, PeppyLauncher},
};
use std::sync::OnceLock;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

static TEST_DATA_DIR: OnceLock<TempDir> = OnceLock::new();

fn init_test_data_dir() {
    let dir = TEST_DATA_DIR.get_or_init(|| tempfile::tempdir().expect("test data dir"));
    config::consts::set_peppy_data_dir_override(dir.path().to_path_buf());
}

/// Returns a minimal master/root node configuration for tests.
/// The master node is the required root of every NodeStack.
pub fn master_node_config() -> NodeConfig {
    init_test_data_dir();
    NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "master",
                tag: "1.0.0",
                start_cmd: ["master"]
            }
        }"#,
    )
    .expect("parse master node config")
}

pub fn node_config(name: &str, tag: &str, deps: &[(&str, &str)]) -> NodeConfig {
    init_test_data_dir();
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
                manifest: {{ name: "{name}", tag: "{tag}", start_cmd: ["{name}"] }},
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
    init_test_data_dir();
    let instance = DeploymentInstance {
        instance_id: Name::new("default").unwrap(),
        arguments: Default::default(),
    };

    Deployment {
        name: config::peppy_config::Name::new(name).unwrap(),
        source,
        tag: tag.to_string(),
        optional,
        instances: vec![instance],
    }
}

pub fn write_config(path: PathBuf, launcher_config: PeppyLauncher) -> PathBuf {
    init_test_data_dir();
    let content = serde_json5::to_string(&launcher_config).expect("serialize config");
    fs::create_dir_all(path.parent().expect("dir")).expect("create config directory");
    fs::write(&path, content).expect("write config");
    path
}

pub fn write_config_str(path: PathBuf, content: &str) -> PathBuf {
    init_test_data_dir();
    fs::create_dir_all(path.parent().expect("dir")).expect("create config directory");
    fs::write(&path, content).expect("write config");
    path
}
