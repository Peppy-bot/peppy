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

    // TODO create the pixi venv and add the peppycl lib to it

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_gitignore(node_path: &Path, lang: &str) -> Result<(), NodeCreationError> {
    let project_root = std::env::current_dir()?;
    let template_path = match lang {
        "python" => project_root
            .join("templates")
            .join("gitignore")
            .join("py.gitignore.j2"),
        "rust" => project_root
            .join("templates")
            .join("gitignore")
            .join("rust.gitignore.j2"),
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
