use crate::Result;
use crate::commands::node::types::NodeName;
use askama::Template;
use std::fs;
use std::path::Path;

#[derive(Template)]
#[template(path = "dependencies/main.rs.j2")]
struct MainRsTemplate;

#[derive(Template)]
#[template(path = "dependencies/Cargo.toml.j2")]
struct CargoTomlTemplate<'a> {
    node_name: &'a str,
    description: &'a str,
}

pub fn add_rust_node_config(node_name: &NodeName, to_path: &Path, description: &str) -> Result<()> {
    // Create src directory
    let src_dir = to_path.join("src");
    fs::create_dir_all(&src_dir)?;

    // Create main.rs from template
    let main_template = MainRsTemplate;
    let main_content = main_template.render().map_err(std::io::Error::other)?;

    let main_rs_path = src_dir.join("main.rs");
    fs::write(main_rs_path, main_content)?;

    // Create Cargo.toml from template
    let cargo_template = CargoTomlTemplate {
        node_name: node_name.as_str(),
        description,
    };
    let cargo_content = cargo_template.render().map_err(std::io::Error::other)?;

    let cargo_toml_path = to_path.join("Cargo.toml");
    fs::write(cargo_toml_path, cargo_content)?;

    std::process::Command::new("cargo")
        .arg("add")
        .arg("tokio")
        .arg("--features")
        .arg("full")
        .current_dir(to_path)
        .output()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_rust_node_config() {
        use tempfile::TempDir;
        let node_name = NodeName::new("test_node").unwrap();
        let temp_dir = TempDir::new().unwrap();
        let description = "A description";

        let result = add_rust_node_config(&node_name, temp_dir.path(), description);
        assert!(result.is_ok());

        // Verify the expected Rust crate files were created
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        let src_dir = temp_dir.path().join("src");
        let main_rs = src_dir.join("main.rs");

        // Check that Cargo.toml exists
        assert!(cargo_toml.exists(), "Expected Cargo.toml to be created");

        // Check that the src directory was created
        assert!(src_dir.exists(), "Expected src directory to be created");
        assert!(src_dir.is_dir(), "src should be a directory");

        // Check that main.rs exists in the src directory
        assert!(main_rs.exists(), "Expected main.rs in src directory");

        // Check that Cargo.toml contains peppylib dependency
        let cargo_content = fs::read_to_string(&cargo_toml).unwrap();
        assert!(
            cargo_content.contains("peppylib"),
            "Expected peppylib dependency in Cargo.toml"
        );
        assert!(
            cargo_content.contains("[dependencies]"),
            "Expected dependencies section in Cargo.toml"
        );

        // Run cargo build to verify the crate compiles
        let output = std::process::Command::new("cargo")
            .arg("build")
            .current_dir(temp_dir.path())
            .output()
            .expect("Failed to execute cargo build");

        assert!(
            output.status.success(),
            "cargo build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
