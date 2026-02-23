use config::{
    consts::PeppyDirs,
    node::{NodeConfig, NodeConfigParser},
    peppy_config::{Deployment, DeploymentInstance, DeploymentSource, Name, PeppyLauncher},
};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

pub fn init_test_data_dir() -> (TempDir, PeppyDirs) {
    let dir = tempfile::tempdir().expect("test data dir");
    let peppy_dirs = PeppyDirs::new(dir.path());
    (dir, peppy_dirs)
}

/// Returns a minimal daemon/root node configuration for tests.
/// The daemon node is the required root of every NodeStack.
pub fn daemon_node_config() -> NodeConfig {
    NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "daemon",
                tag: "1.0.0",
                language: "rust",
                start_cmd: ["daemon"]
            }
        }"#,
    )
    .expect("parse daemon node config")
}

pub fn deployment(source: DeploymentSource) -> Deployment {
    let instance = DeploymentInstance {
        instance_id: Name::new("default").unwrap(),
        arguments: Default::default(),
        env_vars: Default::default(),
    };

    Deployment {
        source,
        instances: vec![instance],
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
