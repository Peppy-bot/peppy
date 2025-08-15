use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use askama::Template;
use thiserror::Error;

use crate::commands::deps;

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

#[derive(Template)]
#[template(path = "gitignore/py.gitignore.j2")]
struct PythonGitignoreTemplate;

#[derive(Template)]
#[template(path = "gitignore/rust.gitignore.j2")]
struct RustGitignoreTemplate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    Rust,
}

impl FromStr for Language {
    type Err = NodeCreationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "python" => Ok(Language::Python),
            "rust" => Ok(Language::Rust),
            _ => Err(NodeCreationError::UnsupportedLanguage),
        }
    }
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
    let language = Language::from_str(lang)?;

    let node_path = match to_dir {
        Some(dir) => dir,
        None => std::env::current_dir()
            .map_err(|e| NodeCreationError::CurrentDir(e))?
            .join(node_name),
    };

    let current_dir = std::env::current_dir().map_err(|e| NodeCreationError::CurrentDir(e))?;
    if !current_dir.join("peppy.star").exists() {
        return Err(NodeCreationError::RootConfigurationNotFound);
    }

    fs::create_dir_all(&node_path)?;

    create_gitignore(&node_path, language)?;
    create_pixi_toml(&node_path, &node_name, language)?;
    create_peppy_config(&node_path, &node_name)?;

    match language {
        Language::Python => deps::create_peppycl_py_dep(&node_path)?,
        Language::Rust => deps::create_peppycl_rust_crate(&node_path)?,
    }

    // TODO add the dep to pixi/Cargo.toml

    println!("Created node '{}' at: {}", node_name, node_path.display());

    Ok(())
}

fn create_gitignore(node_path: &Path, lang: Language) -> Result<(), NodeCreationError> {
    let gitignore_content = match lang {
        Language::Python => {
            let template = PythonGitignoreTemplate;
            template.render().map_err(|e| {
                NodeCreationError::DirectoryCreation(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to render Python gitignore template: {}", e),
                ))
            })?
        }
        Language::Rust => {
            let template = RustGitignoreTemplate;
            template.render().map_err(|e| {
                NodeCreationError::DirectoryCreation(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to render Rust gitignore template: {}", e),
                ))
            })?
        }
    };

    let gitignore_path = node_path.join(".gitignore");
    let mut file = fs::File::create(&gitignore_path)?;
    file.write_all(gitignore_content.as_bytes())?;
    Ok(())
}

fn create_pixi_toml(
    node_path: &Path,
    node_name: &str,
    lang: Language,
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
    fn test_language_from_str() {
        assert_eq!(Language::from_str("python").unwrap(), Language::Python);
        assert_eq!(Language::from_str("rust").unwrap(), Language::Rust);
        assert!(Language::from_str("javascript").is_err());
        assert!(Language::from_str("").is_err());
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
        let lang = Language::Python;

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

        let result = create_gitignore(temp_dir.path(), Language::Python);
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

        let result = create_gitignore(temp_dir.path(), Language::Rust);
        assert!(result.is_ok());

        let gitignore_path = temp_dir.path().join(".gitignore");
        assert!(gitignore_path.exists());

        let content = fs::read_to_string(gitignore_path).unwrap();
        assert!(content.contains("/target/"));
        assert!(content.contains(".pixi"));
    }
}
