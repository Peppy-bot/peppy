use std::fs;
use std::io::Write;
use std::path::Path;

pub fn create_peppycl_rust_crate(to_path: &Path) -> Result<(), std::io::Error> {
    // Create src directory
    let src_dir = to_path.join("src");
    fs::create_dir_all(&src_dir)?;

    // Create lib.rs from template
    let project_root = std::env::current_dir()?;
    let lib_template_path = project_root.join("templates/dependencies/lib.rs.j2");
    let lib_template_content = fs::read_to_string(&lib_template_path)?;

    let lib_rs_path = src_dir.join("lib.rs");
    let mut lib_rs = fs::File::create(lib_rs_path)?;
    lib_rs.write_all(lib_template_content.as_bytes())?;

    // Create Cargo.toml from template
    let cargo_template_path = project_root.join("templates/dependencies/Cargo.toml.j2");
    let cargo_template_content = fs::read_to_string(&cargo_template_path)?;

    let cargo_toml_path = to_path.join("Cargo.toml");
    let mut cargo_toml = fs::File::create(cargo_toml_path)?;
    cargo_toml.write_all(cargo_template_content.as_bytes())?;

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