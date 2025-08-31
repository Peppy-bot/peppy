use config_internal::parse_starlark_config;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_parse_peppy_star_config() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("peppy.star");

    // Write a test configuration based on the example
    let config_content = r#"
def create_root_node():
    return struct(
        namespace = "/",
        version = "0.1.0",
        auto_start = True,
        respawn = False,
        respawn_delay = 2,
        publishes = [],
        subscribes = [],
        services = [],
        actions = [],
        depends_on = [],
        parameters = struct(),
        qos_profile = "default",
        resources = struct(
            max_memory_mb = 512,
            cpu_affinity = [],
        ),
        logging = struct(
            level = "info",
            to_file = False,
            file_path = "",
        ),
        init_script = "",
        diagnostics = struct(
            enabled = True,
            publish_rate_hz = 1,
        ),
    )

root_node = create_root_node()
"#;

    fs::write(&config_path, config_content).unwrap();

    // Parse the configuration
    let config = parse_starlark_config(config_path).unwrap();

    // Verify the parsed values
    assert_eq!(config.namespace, "/");
    assert_eq!(config.version, "0.1.0");
    assert_eq!(config.auto_start, true);
    assert_eq!(config.respawn, false);
    assert_eq!(config.respawn_delay, 2.0);
    assert_eq!(config.qos_profile, "default");
    assert_eq!(config.resources.max_memory_mb, 512);
    assert_eq!(config.logging.level, "info");
    assert_eq!(config.logging.to_file, false);
    assert_eq!(config.diagnostics.enabled, true);
    assert_eq!(config.diagnostics.publish_rate_hz, 1.0);
}
