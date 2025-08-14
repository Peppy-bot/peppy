use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use askama::Template;
use thiserror::Error;

#[derive(Template)]
#[template(path = "pixi.toml.j2")]
struct PixiTomlTemplate<'a> {
    node_name: &'a str,
}

#[derive(Template)]
#[template(path = "peppy_new_node.star.j2")]
struct PeppyNodeTemplate<'a> {
    name: &'a str,
}

#[derive(Error, Debug)]
pub enum NodeCreationError {
    #[error("Failed to create directory: {0}")]
    DirectoryCreation(#[from] std::io::Error),

    #[error("Failed to get current directory")]
    CurrentDir(std::io::Error),

    #[error("Root configuration not found")]
    RootConfigurationNotFound,

    #[error("Unsupported configuration language. Supported options are 'python'/'rust'")]
    UnsupportedLanguage,
}

/// Creates a new node and updates the peppy.star configuration file where the command is run
pub fn create(
    node_name: &str,
    lang: &str,
    to_dir: Option<PathBuf>,
) -> Result<(), NodeCreationError> {
    let node_path = match to_dir {
        Some(dir) => dir,
        None => std::env::current_dir()
            .map_err(|e| NodeCreationError::CurrentDir(e))?
            .join(node_name),
    };

    if !matches!(lang, "python" | "rust") {
        return Err(NodeCreationError::UnsupportedLanguage);
    }

    let current_dir = std::env::current_dir().map_err(|e| NodeCreationError::CurrentDir(e))?;
    if !current_dir.join("peppy.star").exists() {
        return Err(NodeCreationError::RootConfigurationNotFound);
    }

    fs::create_dir_all(&node_path)?;

    create_gitignore(&node_path, &lang)?;
    create_pixi_toml(&node_path, &node_name, &lang)?;
    create_peppy_config(&node_path, &node_name)?;

    if lang == "python" {
        create_peppycl_py_dep(&node_path);
    } else if lang == "rust" {
        create_peppycl_rust_crate(&node_path);
    }

    // TODO add the dep to pixi/Cargo.toml

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_peppycl_py_dep(to_path: &Path) -> Result<(), std::io::Error> {
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

fn create_peppycl_rust_crate(to_path: &Path) -> Result<(), std::io::Error> {
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

fn create_gitignore(node_path: &Path, lang: &str) -> Result<(), NodeCreationError> {
    let project_root = std::env::current_dir()?;
    let template_path = match lang {
        "python" => project_root.join("templates/gitignore/py.gitignore.j2"),
        "rust" => project_root.join("templates/gitignore/rust.gitignore.j2"),
        _ => {
            return Err(NodeCreationError::DirectoryCreation(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Unsupported language: {}", lang),
            )));
        }
    };

    let template_content = fs::read_to_string(&template_path)?;

    let gitignore_path = node_path.join(".gitignore");
    let mut file = fs::File::create(&gitignore_path)?;
    file.write_all(template_content.as_bytes())?;
    Ok(())
}

fn create_pixi_toml(
    node_path: &Path,
    node_name: &str,
    lang: &str,
) -> Result<(), NodeCreationError> {
    let pixi_toml_path = node_path.join("pixi.toml");
    let mut file = fs::File::create(pixi_toml_path)?;

    let template = PixiTomlTemplate { node_name };
    let pixi_content = template.render().map_err(|e| {
        NodeCreationError::DirectoryCreation(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to render pixi template: {}", e),
        ))
    })?;

    file.write_all(pixi_content.as_bytes())?;

    Ok(())
}

fn create_peppy_config(node_path: &Path, node_name: &str) -> Result<(), NodeCreationError> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;

    let template = PeppyNodeTemplate { name: node_name };
    let peppy_content = template.render().map_err(|e| {
        NodeCreationError::DirectoryCreation(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to render peppy template: {}", e),
        ))
    })?;

    file.write_all(peppy_content.as_bytes())?;

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
        assert!(
            cargo_toml.exists(),
            "Expected Cargo.toml to be created"
        );

        // Check that the src directory was created
        assert!(
            src_dir.exists(),
            "Expected src directory to be created"
        );
        assert!(src_dir.is_dir(), "src should be a directory");

        // Check that lib.rs exists in the src directory
        assert!(
            lib_rs.exists(),
            "Expected lib.rs in src directory"
        );
    }

    #[test]
    fn test_check_root_node_config_missing() {
        let result = create("video_node", "python", None);
        assert!(matches!(
            result,
            Err(NodeCreationError::RootConfigurationNotFound)
        ))
    }

    #[test]
    fn test_create_peppy_config() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";

        let result = create_peppy_config(temp_dir.path(), node_name);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.star");
        assert!(peppy_path.exists());

        let content = fs::read_to_string(peppy_path).unwrap();
        assert!(content.contains(node_name));
    }

    #[test]
    fn test_create_pixi_toml() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";
        let lang = "python";

        let result = create_pixi_toml(temp_dir.path(), node_name, lang);
        assert!(result.is_ok());

        let pixi_path = temp_dir.path().join("pixi.toml");
        assert!(pixi_path.exists());

        let content = fs::read_to_string(pixi_path).unwrap();
        assert!(content.contains(node_name));
    }

    #[test]
    fn test_create_gitignore_python() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_gitignore(temp_dir.path(), "python");
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("__pycache__"));
        assert!(content.contains(".pixi"));
    }

    #[test]
    fn test_create_gitignore_rust() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_gitignore(temp_dir.path(), "rust");
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("/target/"));
        assert!(content.contains(".pixi"));
    }

    #[test]
    fn test_create_gitignore_invalid_language() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        let result = create_gitignore(temp_dir.path(), "javascript");
        assert!(result.is_err());

        // Verify no gitignore file was created
        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(!gitignore_path.exists());
    }
}
