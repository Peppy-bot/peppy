use config::parse_yaml_config;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_parse_peppy_yaml_config() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("peppy.yaml");

    // Write a test configuration based on the example
    let config_content = r#"
node_config:
  name: "<root_node>"
  namespace: "/"
  version: "0.1.0"
  auto_start: true
  respawn: false
  respawn_delay: 2.0

node_parameters:

exposes:
  topics: []
  services: []
  actions: []

resources:
  max_memory_mb: 512
  cpu_affinity: []

logging:
  min_level: "info"
  file_path: "/var/log/peppy/peppy_root.log"
  max_file_size_mb: 100
  format: "text"
"#;

    fs::write(&config_path, config_content).unwrap();

    // Parse the configuration
    let config = parse_yaml_config(config_path).unwrap();

    // Verify the parsed values
    assert_eq!(config.node_config.name, "<root_node>");
    assert_eq!(config.node_config.namespace, "/");
    assert_eq!(config.node_config.version, "0.1.0");
    assert_eq!(config.node_config.auto_start, true);
    assert_eq!(config.node_config.respawn, false);
    assert_eq!(config.node_config.respawn_delay, 2.0);
    assert_eq!(config.resources.max_memory_mb, 512);
    assert_eq!(config.logging.min_level, "info");
    assert_eq!(config.logging.file_path, "/var/log/peppy/peppy_root.log");
    assert_eq!(config.logging.max_file_size_mb, 100);
    assert_eq!(config.logging.format, "text");
}
