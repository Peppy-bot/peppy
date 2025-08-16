use askama::Template;
use std::fs;
use std::path::Path;

#[derive(Template)]
#[template(path = "dependencies/lib.rs.j2")]
struct LibRsTemplate;

#[derive(Template)]
#[template(path = "dependencies/Cargo.toml.j2")]
struct CargoTomlTemplate;

pub fn create_peppycl_rust_crate(to_path: &Path) -> Result<(), std::io::Error> {
    // Create src directory
    let src_dir = to_path.join("src");
    fs::create_dir_all(&src_dir)?;

    // Create lib.rs from template
    let lib_template = LibRsTemplate;
    let lib_content = lib_template
        .render()
        .map_err(std::io::Error::other)?;

    let lib_rs_path = src_dir.join("lib.rs");
    fs::write(lib_rs_path, lib_content)?;

    // Create Cargo.toml from template
    let cargo_template = CargoTomlTemplate;
    let cargo_content = cargo_template
        .render()
        .map_err(std::io::Error::other)?;

    let cargo_toml_path = to_path.join("Cargo.toml");
    fs::write(cargo_toml_path, cargo_content)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_peppycl_rust_crate() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_peppycl_rust_crate(temp_dir.path());
        assert!(result.is_ok());

        // Verify the expected Rust crate files were created
        let cargo_toml = temp_dir.path().join("Cargo.toml");
        let src_dir = temp_dir.path().join("src");
        let lib_rs = src_dir.join("lib.rs");

        // Check that Cargo.toml exists
        assert!(cargo_toml.exists(), "Expected Cargo.toml to be created");

        // Check that the src directory was created
        assert!(src_dir.exists(), "Expected src directory to be created");
        assert!(src_dir.is_dir(), "src should be a directory");

        // Check that lib.rs exists in the src directory
        assert!(lib_rs.exists(), "Expected lib.rs in src directory");
    }
}
