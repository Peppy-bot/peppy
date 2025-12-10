use crate::error::{Error, Result};
use askama::Template;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::commands::node::types::NodeName;

#[derive(Template)]
#[template(path = "dependencies/pyproject.toml.j2")]
struct PyProjectTomlTemplate<'a> {
    node_name: &'a str,
}

pub fn add_python_node_config(node_name: &NodeName, to_path: &Path) -> Result<()> {
    // Create the package directory
    let package_dir = to_path.join(node_name.as_str());
    fs::create_dir_all(&package_dir)?;

    // Create __init__.py in the package directory
    let init_py_path = package_dir.join("__init__.py");
    let mut init_file = fs::File::create(init_py_path)?;
    init_file.write_all(b"# Node Python package\n")?;

    // Create pyproject.toml from template
    let pyproject_template = PyProjectTomlTemplate {
        node_name: node_name.as_str(),
    };
    let pyproject_content = pyproject_template
        .render()
        .map_err(|e| Error::Askama(e.to_string()))?;

    let pyproject_toml_path = to_path.join("pyproject.toml");
    fs::write(pyproject_toml_path, pyproject_content)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_python_node_config() {
        use tempfile::TempDir;
        let node_name = NodeName::new("test_node").unwrap();
        let temp_dir = TempDir::new().unwrap();

        let result = add_python_node_config(&node_name, temp_dir.path());
        assert!(result.is_ok());

        // Verify the expected Python dependency files were created
        let pyproject_toml = temp_dir.path().join("pyproject.toml");
        let node_dir = temp_dir.path().join(node_name.as_str());
        let init_py = node_dir.join("__init__.py");

        // Check that at least one of the standard Python package files exists
        assert!(
            pyproject_toml.exists(),
            "Expected either pyproject.toml to be created"
        );

        // Check that the node package directory was created
        assert!(node_dir.exists(), "Expected node directory to be created");
        assert!(node_dir.is_dir(), "node should be a directory");

        // Check that __init__.py exists in the package directory
        assert!(
            init_py.exists(),
            "Expected __init__.py in peppylib directory"
        );
    }
}
