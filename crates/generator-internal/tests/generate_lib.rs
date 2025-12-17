use config::peppy_config::BuildSystem;
use generator::generate_lib_for_build_system;
use std::fs;
use tempfile::TempDir;

const PEPPY_JSON5_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "test_node",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  },
  interfaces: {
    exposes: {
      topics: [
        {
          name: "test_topic",
          qos_profile: "sensor_data",
          message_format: {
            value: "u32",
            timestamp: "time"
          }
        }
      ],
      services: [
        {
          name: "test_service",
          request_message_format: {
            input: "string"
          },
          response_message_format: {
            output: "string",
            success: "bool"
          }
        }
      ]
    }
  },
  logging: {
    min_level: "info"
  }
}"#;

fn run_generate_peppygen_lib_test(build_system: BuildSystem) -> (TempDir, std::path::PathBuf) {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Write the peppy.json5 config
    let config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    fs::write(&config_path, PEPPY_JSON5_CONFIG).expect("failed to write peppy.json5");

    // Generate the library
    generate_lib_for_build_system(build_system, node_dir).expect("failed to generate library");

    // Verify the generated library structure exists
    let peppygen_dir = node_dir.join(".peppy/libs/peppygen");
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    // Check that the fingerprint was created
    let fingerprint_path = peppygen_dir.join(config::consts::NODE_CONFIG_FINGERPRINT_FILE);
    assert!(
        fingerprint_path.exists(),
        "fingerprint file should exist at {}",
        fingerprint_path.display()
    );
    let fingerprint = fs::read_to_string(&fingerprint_path).expect("failed to read fingerprint");
    assert!(
        !fingerprint.trim().is_empty(),
        "fingerprint should not be empty"
    );

    (temp_dir, peppygen_dir)
}

#[test]
fn generate_peppygen_lib_minimal_config() {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Minimal config with no interfaces
    let minimal_config = r#"{
      schema_version: 1,
      manifest: {
        name: "minimal_node",
        tag: "0.1.0",
        launch_cmd: ["cargo", "run"]
      }
    }"#;

    let config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    fs::write(&config_path, minimal_config).expect("failed to write peppy.json5");

    // Generate should succeed even with no interfaces
    generate_lib_for_build_system(BuildSystem::Cargo, node_dir)
        .expect("failed to generate library for minimal config");

    // Verify the generated library exists
    let peppygen_dir = node_dir.join(".peppy/libs/peppygen");
    assert!(peppygen_dir.exists(), "peppygen directory should exist");
}

#[test]
fn generate_peppygen_lib_cargo() {
    let (temp_dir, peppygen_dir) = run_generate_peppygen_lib_test(BuildSystem::Cargo);
    let node_dir = temp_dir.path();

    // Check that Cargo.toml was generated
    let cargo_toml = peppygen_dir.join("Cargo.toml");
    assert!(
        cargo_toml.exists(),
        "Cargo.toml should exist at {}",
        cargo_toml.display()
    );

    // Check that src/lib.rs was generated
    let lib_rs = peppygen_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "src/lib.rs should exist at {}",
        lib_rs.display()
    );

    // Check that exposed topics module was generated
    let exposed_topics_dir = peppygen_dir.join("src/exposed_topics");
    assert!(
        exposed_topics_dir.exists(),
        "exposed_topics directory should exist at {}",
        exposed_topics_dir.display()
    );

    // Check that exposed services module was generated
    let exposed_services_dir = peppygen_dir.join("src/exposed_services");
    assert!(
        exposed_services_dir.exists(),
        "exposed_services directory should exist at {}",
        exposed_services_dir.display()
    );

    // Check that the Cargo.toml in node_dir has peppygen as dependency and points to the peppygen_dir path
    let node_cargo_toml = node_dir.join("Cargo.toml");
    assert!(
        node_cargo_toml.exists(),
        "Cargo.toml should be created in node_dir at {}",
        node_cargo_toml.display()
    );

    let cargo_contents = fs::read_to_string(&node_cargo_toml).expect("failed to read Cargo.toml");
    let cargo_doc: toml::Value =
        toml::from_str(&cargo_contents).expect("Cargo.toml should be valid TOML");

    // Verify package name matches the node name from config
    let package_name = cargo_doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .expect("Cargo.toml should have package.name");
    assert_eq!(
        package_name, "test_node",
        "package name should match node name from config"
    );

    // Verify peppygen dependency exists and points to the correct path
    let peppygen_dep = cargo_doc
        .get("dependencies")
        .and_then(|d| d.get("peppygen"))
        .expect("Cargo.toml should have peppygen dependency");

    let peppygen_path = peppygen_dep
        .get("path")
        .and_then(|p| p.as_str())
        .expect("peppygen dependency should have a path");
    assert_eq!(
        peppygen_path, ".peppy/libs/peppygen",
        "peppygen dependency path should point to .peppy/libs/peppygen"
    );
}

#[test]
#[ignore = "Python generator not yet implemented"]
fn generate_peppygen_lib_uv() {
    let (_temp_dir, peppygen_dir) = run_generate_peppygen_lib_test(BuildSystem::Uv);

    // Check that pyproject.toml was generated
    let pyproject_toml = peppygen_dir.join("pyproject.toml");
    assert!(
        pyproject_toml.exists(),
        "pyproject.toml should exist at {}",
        pyproject_toml.display()
    );

    // Check that peppygen/__init__.py was generated
    let init_py = peppygen_dir.join("peppygen/__init__.py");
    assert!(
        init_py.exists(),
        "peppygen/__init__.py should exist at {}",
        init_py.display()
    );
}

#[test]
fn generate_peppygen_lib_missing_config() {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Try to generate without a peppy.json5 - should fail
    let result = generate_lib_for_build_system(BuildSystem::Cargo, node_dir);
    assert!(result.is_err(), "should fail when peppy.json5 is missing");
}
