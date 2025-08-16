use std::fs;
use std::io::Write;
use std::path::Path;

pub fn create_peppycl_py_dep(to_path: &Path) -> Result<(), std::io::Error> {
    // Create the peppycl package directory
    let peppycl_dir = to_path.join("peppycl");
    fs::create_dir_all(&peppycl_dir)?;

    // Create __init__.py in the peppycl directory
    let init_py_path = peppycl_dir.join("__init__.py");
    let mut init_file = fs::File::create(init_py_path)?;
    init_file.write_all(b"# Peppycl Python package\n")?;

    // Create pyproject.toml from template
    let project_root = std::env::current_dir()?;
    let template_path = project_root.join("templates/dependencies/pyproject.toml.j2");
    let template_content = fs::read_to_string(&template_path)?;

    let pyproject_toml_path = to_path.join("pyproject.toml");
    let mut pyproject_file = fs::File::create(pyproject_toml_path)?;
    pyproject_file.write_all(template_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_peppycl_py_dep() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_peppycl_py_dep(temp_dir.path());
        assert!(result.is_ok());

        // Verify the expected Python dependency files were created
        let pyproject_toml = temp_dir.path().join("pyproject.toml");
        let peppycl_dir = temp_dir.path().join("peppycl");
        let init_py = peppycl_dir.join("__init__.py");

        // Check that at least one of the standard Python package files exists
        assert!(
            pyproject_toml.exists(),
            "Expected either pyproject.toml to be created"
        );

        // Check that the peppycl package directory was created
        assert!(
            peppycl_dir.exists(),
            "Expected peppycl directory to be created"
        );
        assert!(peppycl_dir.is_dir(), "peppycl should be a directory");

        // Check that __init__.py exists in the package directory
        assert!(
            init_py.exists(),
            "Expected __init__.py in peppycl directory"
        );
    }
}
